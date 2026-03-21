// =============================================================================
//  src/bin/GCS.rs   —   Ground Control Station (GCS)
//  CT087-3-3 Real-Time Systems  |  Student B
//
//  Run:   cargo run --bin GCS --release
//         (start OCS first — it binds UDP ports before GCS connects)
//
//  ARCHITECTURE OVERVIEW:
//  This is a SOFT Real-Time System built on Tokio (async/await).
//  As covered in Lab 6, async tasks are ideal for I/O-heavy workloads
//  with soft deadlines. GCS has soft deadlines (3ms decode, 2ms dispatch)
//  and spends most of its time waiting on network I/O — async is the right fit.
//
//  IPC (Inter-Process Communication):
//    GCS listens for OCS telemetry on   0.0.0.0:9000
//    GCS sends commands to OCS at       127.0.0.1:9001
//
//  SERDE / SERDE_JSON:
//  Incoming OCS payloads are deserialised with serde_json::from_str::<OcsMessage>().
//  Outgoing GCS commands are serialised with serde_json::to_string(&GcsCommand{}).
//  Both types share the same wire format with OCS.
//
//  LAB REFERENCE MAP:
//  ─────────────────────────────────────────────────────────────
//  Lab 1  const, structs, enums, match, Vec, mut
//  Lab 2  Arc<Mutex<T>>, Arc::clone()
//  Lab 3  Instant::now() + .elapsed(), min/max/avg stat helper
//  Lab 6  #[tokio::main], tokio::spawn, sleep().await, heartbeat,
//         spawn_blocking — CPU decode offloaded from async runtime
//  Lab 7  PhantomData<S> typestate: GcsMode<Normal>/GcsMode<FaultLocked>
//         dispatch_command() only compiles with GcsMode<Normal>
//         enum OcsFaultKind + match, consecutive-miss counter reset
//  Lab 8  tokio::select!, CancellationToken, supervisor restart loop,
//         fragile task + match handle.await { Err(e) if e.is_panic() }
//  Lab 9  tokio::net::UdpSocket — recv_from / send_to (async)
//  Lab 11 tokio::sync::mpsc channel, Arc<Mutex<Vec>> command queue
// =============================================================================

// ── Standard Library Imports ─────────────────────────────────────────────────
use std::fs::{File, OpenOptions};           // for runtime log file
use std::io::Write;                         // for writing log lines
use std::marker::PhantomData;               // for typestate pattern (Lab 7)
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};               // for shared ownership across tasks (Lab 2)
use std::time::{Duration, Instant};         // Instant only — simulation-relative time used throughout

// ── Tokio Async Imports ───────────────────────────────────────────────────────
use tokio::net::UdpSocket;                  // async UDP socket (Lab 9)
use tokio::sync::mpsc;                      // async multi-producer single-consumer channel (Lab 11)
use tokio::time::sleep;                     // async sleep — does NOT block the thread (Lab 6)
use tokio_util::sync::CancellationToken;    // graceful shutdown token (Lab 8)

// ── External Crate Imports ────────────────────────────────────────────────────
use rand;                                   // random number generation (Lab 8 fault simulation)
use serde::{Deserialize, Serialize};        // automatic JSON serialisation/deserialisation
use serde_json;                             // JSON encode/decode for UDP payloads


// =============================================================================
//  PART 0 — CONSTANTS   (Lab 1: const keyword)
//
//  Constants must have an explicit type, cannot be mutable, and exist
//  for the entire lifetime of the program.
// =============================================================================

// ── Rate Monotonic Command Periods (milliseconds) ─────────────────────────────
// Shorter period = higher RM priority (Week 9 — Rate Monotonic Scheduling)
const THERMAL_COMMAND_PERIOD:       u64 = 50;   // RM P1 — fastest, highest priority
const ACCELEROMETER_COMMAND_PERIOD: u64 = 120;  // RM P2
const GYROSCOPE_COMMAND_PERIOD:     u64 = 333;  // RM P3 — slowest, lowest priority

// ── Timing Deadlines (milliseconds) ──────────────────────────────────────────
const DECODE_DEADLINE:      u64 = 3;    // assignment spec: telemetry must decode within 3ms
const DISPATCH_DEADLINE:    u64 = 2;    // assignment spec: urgent commands must dispatch within 2ms
const FAULT_RESPONSE_LIMIT: u64 = 100;  // assignment spec: interlock must engage within 100ms

// ── Loss of Contact and Watchdog ──────────────────────────────────────────────
const LOSS_OF_CONTACT_MISS_THRESHOLD: u32 = 3;
// assignment spec: simulate loss of contact after 3 consecutive missed packets

const TELEMETRY_WATCHDOG: u64 = 800;
// 800ms = 8x the OCS thermal period (100ms).
// If no packet arrives within 8 cycles, that sensor channel is considered silent.

const REREQUEST_INTERVAL: u64 = 500;
// Check every 500ms — fast enough to detect silence, slow enough to not flood the system.

// ── Jitter Warning Threshold (microseconds) ───────────────────────────────────
const UPLINK_JITTER_LIMIT: i64 = 2000;
// 2000 microseconds = 2ms. Matches the 2ms dispatch deadline (Week 9 — RMS).

// ── Simulation Config ─────────────────────────────────────────────────────────
const SIMULATION_DURATION: u64 = 180;  // 3 minutes — matches OCS simulation duration

// ── Network Addresses ─────────────────────────────────────────────────────────
const GCS_TELEMETRY_BIND:  &str = "0.0.0.0:9000";   // GCS listens for OCS telemetry here
const OCS_COMMAND_ADDRESS: &str = "127.0.0.1:9001";  // GCS sends commands to OCS here


// ── Runtime Log File ──────────────────────────────────────────────────────────
const RUNTIME_LOG_FILE: &str = "gcs_runtime.log";
// Detailed per-packet events go here — keeps the terminal readable


// =============================================================================
//  SERDE MESSAGE TYPES  (shared wire format with OCS)
//
//  OcsMessage mirrors the enum in OCS.rs exactly — field names, variant names,
//  and the #[serde(tag = "tag")] discriminator all match the wire format.
//  serde_json::from_str::<OcsMessage>(&payload) replaces the old error-prone:
//    if payload.contains("\"tag\":\"alert\"") { ... }
//
//  GcsCommand is serialised with serde_json::to_string(&GcsCommand{...}) and
//  replaces every manual format!("{{\"tag\":\"cmd\",...}}") string.
// =============================================================================

// Incoming telemetry messages from the OCS — enum with one variant per message type
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "tag", rename_all = "snake_case")]
enum OcsMessage
{
    Thermal
    {
        seq:      u64,
        temp:     f64,
        drift_timing: i64,
    },

    Accel
    {
        seq: u64,
        mag: f64,
    },

    Gyro
    {
        generation: u32,
        seq:        u64,
        omega_z:    f64,
    },

    Status
    {
        iter:     u64,
        fill:     f64,
        state:    String,
        drift_timing: i64,
    },

    Downlink
    {
        pkt:      u64,
        bytes:    usize,
        queue_latency: u64,
    },

    // Alert uses #[serde(flatten)] so AlertInfo fields appear directly
    // in the JSON object alongside "event", rather than nested.
    Alert
    {
        event: String,
        #[serde(flatten)]
        info:  AlertInfo,
    },
}

// Optional extra fields for Alert messages.
// We use Option<T> so absent fields are omitted (skip_serializing_if) rather
// than written as null — keeps the wire format clean.
#[derive(Serialize, Deserialize, Debug, Default)]
struct AlertInfo
{
    #[serde(skip_serializing_if = "Option::is_none")]
    pub misses:     Option<u32>,      // for thermal miss alerts

    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,      // for gyro restart alerts

    #[serde(skip_serializing_if = "Option::is_none")]
    pub count:      Option<u32>,      // fault occurrence count

    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault_type: Option<String>,   // e.g. "SensorBusHang", "PowerSpike"
}

// Outgoing command sent to OCS over UDP
// Optional fields are omitted from JSON when None (cleaner wire format)
#[derive(Serialize, Deserialize, Debug)]
struct GcsCommand
{
    pub tag:     String,   // always "cmd"
    pub cmd:     String,   // e.g. "ThermalCheck", "EmergencyHalt"
    pub ts:      u64,      // simulation-relative timestamp in ms

    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority:   Option<u8>,    // RM priority of the issuing task

    #[serde(skip_serializing_if = "Option::is_none")]
    pub iter:       Option<u64>,   // which iteration of the task sent this

    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,   // which generation of the gyroscope dispatcher
}

// Helper: serialise a GcsCommand to a JSON string.
// Logs on error and returns "" so the caller can safely send an empty string.
fn encode_command(command: &GcsCommand) -> String
{
    serde_json::to_string(command)
        .unwrap_or_else(|error|
        {
            println!("[ENCODE] {error}");
            String::new()
        })
}


// =============================================================================
//  LAB 7 — GCS MODE TYPESTATE  (PhantomData<S>)
//
//  This is the same pattern as the Door typestate in Lab 7.
//
//  Door analogy → GcsMode analogy:
//    Door<Locked>       →  GcsMode<Normal>       (commands allowed)
//    Door<Unlocked>     →  GcsMode<FaultLocked>  (commands BLOCKED)
//    door.unlock()      →  mode.lock_for_fault()
//    door.lock()        →  mode.clear_fault()
//
//  The key safety property: dispatch_command() takes &GcsMode<Normal>.
//  Passing a GcsMode<FaultLocked> is a COMPILE ERROR.
//  The compiler literally prevents sending commands during a fault condition.
//
//  Each command task:
//    1. Checks the runtime fault_active flag.
//    2. If clear → constructs GcsMode::<Normal>::new().
//    3. Passes it to dispatch_command() — compile-time proof it's Normal.
// =============================================================================

// The two possible GCS operating modes
struct Normal;
struct FaultLocked;

// Generic GCS mode struct — S is the current mode
struct GcsMode<S>
{
    _state: PhantomData<S>  // zero-size marker; no runtime cost
}

// Methods available ONLY in Normal mode (Lab 7: impl Door<Locked>)
impl GcsMode<Normal>
{
    fn new() -> Self
    {
        GcsMode { _state: PhantomData }
    }

    // Normal → FaultLocked (Lab 7: door.unlock() → Door<Unlocked>)
    fn lock_for_fault(self) -> GcsMode<FaultLocked>
    {
        println!("[GcsMode]  Normal → FaultLocked — commands BLOCKED");
        GcsMode { _state: PhantomData }
    }
}

// Methods available ONLY in FaultLocked mode (Lab 7: impl Door<Unlocked>)
impl GcsMode<FaultLocked>
{
    // FaultLocked → Normal (Lab 7: door.lock() → Door<Locked>)
    fn clear_fault(self) -> GcsMode<Normal>
    {
        println!("[GcsMode]  FaultLocked → Normal — commands RESTORED");
        GcsMode { _state: PhantomData }
    }
    // NOTE: No dispatch_command() here — you CANNOT dispatch during fault.
    // NOTE: No lock_for_fault() here — you cannot double-lock.
}

// This function ONLY accepts GcsMode<Normal>.
// If you try to call it with GcsMode<FaultLocked>, the compiler rejects it.
// This is the same idea as Door<Open> having no .lock() method in Lab 7.
fn dispatch_command(
    _mode:    &GcsMode<Normal>,         // compile-time proof: we are in Normal mode
    cmd_tx:   &mpsc::Sender<UplinkCmd>,
    payload:  String,
    priority: u8,
)
{
    // try_send is non-blocking — if the channel is full, we silently drop
    let _ = cmd_tx.try_send(UplinkCmd
    {
        payload,
        priority,
        created_at: Instant::now(),
    });
}


// =============================================================================
//  DATA TYPES   (Lab 1: structs, enums)
// =============================================================================

// A raw incoming UDP packet, timestamped at arrival before any processing
// Lab 3: we stamp received_at before any processing to measure decode latency
#[derive(Debug)]
struct IncomingPacket
{
    payload:     String,
    received_at: Instant,  // stamped BEFORE processing (Lab 3: latency measurement)
    from:        SocketAddr,
}

// An outgoing command waiting to be sent to OCS
#[derive(Debug, Clone)]
struct UplinkCmd
{
    payload:    String,
    priority:   u8,
    created_at: Instant,  // Lab 3: dispatch latency measured from this
}

// Lab 7: fault classification enum — mirrors SensorError from Lab 7
// classify_event() converts the OCS alert's event string into this typed enum
#[derive(Debug)]
enum OcsFaultKind
{
    ThermalAlert,
    FaultInjected,
    GyroRestart,
    MissionAbort,
    Unknown,
}

// All GCS performance metrics (collected across all tasks)
#[derive(Default)]
struct GcsMetrics
{
    // Telemetry reception
    telemetry_received:   u64,
    missed_packets:       u64,

    // Lab 3: decode and dispatch latency measurement
    decode_latency:    Vec<u64>,
    reception_drift_timing:   Vec<i64>,

    // Command uplink
    commands_sent:        u64,
    commands_rejected:    u64,
    dispatch_latency:  Vec<u64>,
    rejection_log:        Vec<String>,

    // Timing violations
    deadline_violations:  Vec<String>,

    // Lab 2 / Lab 11: jitter per RM task
    thermal_jitter:      Vec<i64>,
    accelerometer_jitter: Vec<i64>, // variable name changed
    gyroscope_jitter:     Vec<i64>, // variable name changed

    // Fault handling
    faults_received:      u64,
    interlock_latency: Vec<u64>,
    critical_alerts:      Vec<String>,

    // CPU estimate
    drift_timing:    Vec<i64>,
    active_ms:   u64,
    elapsed_time:  u64,

    // Lab 8: how many times gyroscope supervisor restarted
    gyroscope_restarts: u32, // variable name changed

    // Component 4 — backlog depth   (lab 2 — Arc<Mutex<T>> shared counter)
    // Tracks how many packets are sitting in the incoming channel unprocessed
    backlog_depth_samples: Vec<usize>,  // snapshot every health check tick
    backlog_peak:          usize,       // highest depth seen during the run
}

// Runtime GCS state shared across tasks
struct GcsState
{
    fault_active:       bool,
    fault_detected_at:  Option<Instant>,
    fault_log:          Vec<String>,

    // Lab 7: consecutive miss counters (same pattern as Lab 7's glitch_count)
    thermal_misses:       u32,
    accelerometer_misses: u32, // variable name changed
    gyroscope_misses:     u32, // variable name changed

    // Watchdog timestamps — updated on each successful telemetry receipt
    last_thermal:       u64,
    last_accelerometer: u64, // variable name changed
    last_gyroscope:     u64, // variable name changed

    loss_of_contact:    bool,
}

// impl Default so we can write GcsState::default() in main
impl Default for GcsState
{
    fn default() -> Self
    {
        GcsState
        {
            fault_active:          false,
            fault_detected_at:     None,
            fault_log:             Vec::new(),
            thermal_misses:        0,
            accelerometer_misses:  0, // variable name changed
            gyroscope_misses:      0, // variable name changed
            last_thermal:          0,
            last_accelerometer:    0, // variable name changed
            last_gyroscope:        0, // variable name changed
            loss_of_contact:       false,
        }
    }
}


// =============================================================================
//  HELPER FUNCTIONS
// =============================================================================

// Shared Arc<Mutex<T>> alias to reduce signature noise (matches OCS style)
type Shared<T> = Arc<Mutex<T>>;

// Returns simulation-relative time in milliseconds from simulation start.
// This replaces Unix time (SystemTime::now()) throughout the GCS.
// Every log line and timestamp is relative to when the simulation began,
// making it directly comparable to OCS log output.
fn simulation_elapsed(simulation_start: &Instant) -> u64
{
    simulation_start.elapsed().as_millis() as u64
}

// Helper: write a detailed runtime log line to the log file only.
// Used for high-frequency per-packet events that would flood the terminal.
fn write_runtime_log(runtime_logger: &Shared<File>, line: &str)
{
    let mut log_file = runtime_logger.lock().unwrap();
    let _ = writeln!(log_file, "{line}");
}

// Helper: print important events to terminal AND write them to the runtime log.
// Used for faults, alerts, deadline violations — events worth seeing live.
fn print_and_log(runtime_logger: &Shared<File>, line: &str)
{
    println!("{line}");
    write_runtime_log(runtime_logger, line);
}

// Pretty-print one row of the metrics table (Lab 3: min/max/avg statistics)
fn print_stat_row(label: &str, samples: &[i64])
{
    if samples.is_empty()
    {
        println!("    {:<36} — no data", label);
        return;
    }

    let minimum_value = samples.iter().min().unwrap();
    let maximum_value = samples.iter().max().unwrap();
    let average_value = samples.iter().sum::<i64>() as f64 / samples.len() as f64;

    println!(
        "    {:<36} n={:>5}  min={:>7}  max={:>7}  avg={:>9.1}",
        label,
        samples.len(),
        minimum_value,
        maximum_value,
        average_value
    );
}

// Lab 7: convert the OCS alert's event string into a typed OcsFaultKind enum.
// This is called AFTER serde has already validated the outer message structure,
// so we only need to match on the inner event string.
fn classify_event(event: &str) -> OcsFaultKind
{
    match event
    {
        "mission_abort" => OcsFaultKind::MissionAbort,
        "thermal_alert" => OcsFaultKind::ThermalAlert,
        "gyro_restart"  => OcsFaultKind::GyroRestart,
        "fault"         => OcsFaultKind::FaultInjected,
        _               => OcsFaultKind::Unknown,
    }
}


// =============================================================================
//  LOG FILE INITIALISER
//
//  Creates/truncates the runtime log file at startup.
//  Detailed per-packet events are written here to keep the terminal readable.
//  Matches the OCS create_runtime_logger() pattern.
// =============================================================================

fn create_runtime_logger() -> Shared<File>
{
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(RUNTIME_LOG_FILE)
        .expect("[LOG] Failed to create runtime log file");

    Arc::new(Mutex::new(log_file))
}


// =============================================================================
//  UDP RECEIVER LOOP   (Lab 9: tokio::net::UdpSocket async recv_from)
//
//  Continuously receives UDP packets from the OCS and forwards them
//  to the telemetry processor via an mpsc channel (Lab 11).
//
//  Lab 6: tokio::select! allows waiting on EITHER the shutdown signal
//  OR a new packet arriving — whichever happens first.
//  This is the non-blocking version of the Lab 9 blocking recv_from().
// =============================================================================

async fn udp_receiver_loop(
    sock:             Arc<UdpSocket>,
    incoming_tx:      mpsc::Sender<IncomingPacket>,  // Lab 11: producer side of channel
    backlog_counter:  Shared<usize>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,             // Lab 8: cancellation signal
)
{
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [UDP Receiver] Listening for OCS telemetry on {GCS_TELEMETRY_BIND}",
            simulation_elapsed(&simulation_start)
        ),
    );

    let mut buf = vec![0u8; 4096];  // 4096 bytes is plenty for a JSON telemetry packet

    loop
    {
        // Lab 8: tokio::select! waits for the FIRST of these two futures to complete
        tokio::select!
        {
            // Branch 1: shutdown was requested
            _ = shutdown.cancelled() =>
            {
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [UDP Receiver] Exit.", simulation_elapsed(&simulation_start)),
                );
                return;
            }

            // Branch 2: a UDP packet arrived
            result = sock.recv_from(&mut buf) =>
            {
                match result
                {
                    Ok((len, from)) =>
                    {
                        // Lab 3: stamp the arrival time BEFORE any processing
                        let received_at = Instant::now();
                        let payload = String::from_utf8_lossy(&buf[..len]).to_string();

                        // Lab 11: send to telemetry processor via channel
                        if incoming_tx.try_send(IncomingPacket { payload, received_at, from }).is_ok()
                        {
                            // lab 2 — increment backlog counter on successful send
                            *backlog_counter.lock().unwrap() += 1;
                        }
                        else
                        {
                            print_and_log(
                                &runtime_logger,
                                &format!(
                                    "[{}ms] [UDP Receiver] Channel full — packet dropped from {from}",
                                    simulation_elapsed(&simulation_start)
                                ),
                            );
                        }
                    }
                    Err(error) => print_and_log(
                        &runtime_logger,
                        &format!(
                            "[{}ms] [UDP Receiver] recv_from error: {error}",
                            simulation_elapsed(&simulation_start)
                        ),
                    ),
                }
            }
        }
    }
}


// =============================================================================
//  UDP SENDER TASK   (Lab 9: send_to + Lab 11: mpsc consumer)
//
//  Consumes UplinkCmd messages from an mpsc channel and sends them
//  to the OCS over UDP. Also measures dispatch latency (Lab 3).
//
//  Lab 6: tokio::select! between shutdown and next command in channel.
// =============================================================================

async fn udp_sender_task(
    mut rx:           mpsc::Receiver<UplinkCmd>,
    sock:             Arc<UdpSocket>,
    metrics:          Shared<GcsMetrics>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,
)
{
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [UDP Sender] Command link established → {OCS_COMMAND_ADDRESS}",
            simulation_elapsed(&simulation_start)
        ),
    );

    loop
    {
        // Lab 8: select on either shutdown or next command
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [UDP Sender] Exit.", simulation_elapsed(&simulation_start)),
                );
                return;
            }

            cmd = rx.recv() =>
            {
                match cmd
                {
                    Some(c) =>
                    {
                        // Lab 3: measure how long the command sat in the queue
                        // .as_micros() used here — dispatch latency is a short duration
                        // where microsecond precision matters
                        let dispatch = c.created_at.elapsed().as_micros() as u64;

                        match sock.send_to(c.payload.as_bytes(), OCS_COMMAND_ADDRESS).await
                        {
                            Ok(_) =>
                            {
                                // Check dispatch deadline
                                if dispatch / 1000 > DISPATCH_DEADLINE
                                {
                                    let violation = format!(
                                        "[{}ms] [DEADLINE] Dispatch {}µs > {}ms",
                                        simulation_elapsed(&simulation_start),
                                        dispatch,
                                        DISPATCH_DEADLINE
                                    );
                                    print_and_log(&runtime_logger, &violation);
                                    metrics.lock().unwrap().deadline_violations.push(violation);
                                }
                                else
                                {
                                    write_runtime_log(
                                        &runtime_logger,
                                        &format!(
                                            "[{}ms] [UDP Sender] Sent: {}",
                                            simulation_elapsed(&simulation_start),
                                            c.payload
                                        ),
                                    );
                                }

                                let mut m = metrics.lock().unwrap();
                                m.commands_sent += 1;
                                m.dispatch_latency.push(dispatch);
                            }
                            Err(error) => print_and_log(
                                &runtime_logger,
                                &format!(
                                    "[{}ms] [UDP Sender] send_to error: {error}",
                                    simulation_elapsed(&simulation_start)
                                ),
                            ),
                        }
                    }
                    None => return,  // channel closed — all senders dropped
                }
            }
        }
    }
}


// =============================================================================
//  PART 1 — TELEMETRY PROCESSOR  (Lab 6: spawn_blocking + serde deserialise)
//
//  Receives raw UDP packets from the udp_receiver_loop via mpsc channel
//  and decodes them into typed OcsMessage values using serde_json.
//
//  KEY LESSON from Lab 6:
//  JSON parsing is CPU-bound synchronous work. Running it inline would
//  block the Tokio worker thread (same problem as heavy_work() in Lab 6).
//  Solution: offload it with spawn_blocking() to a dedicated OS thread,
//  keeping the async runtime free for other tasks.
//
//  After decoding, each variant is handled by a match arm — no more
//  fragile payload.contains("\"tag\":\"alert\"") string checks.
// =============================================================================

// Synchronous decode function — runs inside spawn_blocking (Lab 6).
// serde_json::from_str parses the raw bytes into a typed OcsMessage.
fn decode_packet_sync(payload: String) -> Result<OcsMessage, String>
{
    serde_json::from_str::<OcsMessage>(&payload)
        .map_err(|error| format!("serde parse error: {error}  payload={payload}"))
}

async fn telemetry_processor_task(
    mut rx:           mpsc::Receiver<IncomingPacket>,   // Lab 11: consumer
    state:            Shared<GcsState>,
    metrics:          Shared<GcsMetrics>,
    cmd_tx:           mpsc::Sender<UplinkCmd>,
    backlog_counter:  Shared<usize>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,
)
{
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Telemetry Receiver] Ready.  decode_deadline={}ms  (spawn_blocking + serde_json)",
            simulation_elapsed(&simulation_start),
            DECODE_DEADLINE
        ),
    );

    // Tracks the Instant of the last received packet for reception drift
    let mut last_any_rx: Option<Instant> = None;

    loop
    {
        // Lab 8: select on either shutdown or next incoming packet
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [Telemetry Receiver] Exit.", simulation_elapsed(&simulation_start)),
                );
                return;
            }

            pkt = rx.recv() =>
            {
                match pkt
                {
                    None => return,  // channel closed
                    Some(p) =>
                    {
                        // lab 2 — decrement backlog counter: one packet consumed
                        {
                            let mut depth = backlog_counter.lock().unwrap();
                            if *depth > 0 { *depth -= 1; }
                        }

                        let arrival  = p.received_at;
                        let from     = p.from;
                        let payload  = p.payload.clone();

                        // ── Lab 6: spawn_blocking for CPU-bound JSON parsing ───────────────
                        // decode_packet_sync is a blocking/synchronous function.
                        // Calling it directly inside async would block the Tokio thread.
                        // spawn_blocking moves it to a separate OS thread pool — same
                        // lesson as heavy_work() using spawn_blocking in Lab 6.
                        let decode_result = tokio::task::spawn_blocking(
                            move || decode_packet_sync(payload)
                        ).await;

                        // Lab 3: measure decode latency using .as_micros() for precision
                        // This is one of the cases where microseconds are appropriate —
                        // decode latency is short enough that ms would lose resolution.
                        let decode_time = arrival.elapsed().as_micros() as u64;
                        let decode_time_ms = decode_time / 1000;

                        if decode_time_ms > DECODE_DEADLINE
                        {
                            let violation = format!(
                                "[{}ms] [DEADLINE] Decode {}ms > {}ms  from={from}",
                                simulation_elapsed(&simulation_start),
                                decode_time_ms,
                                DECODE_DEADLINE
                            );
                            print_and_log(&runtime_logger, &violation);
                            metrics.lock().unwrap().deadline_violations.push(violation);
                        }

                        // Unwrap the JoinHandle result first, then the parse result
                        let msg = match decode_result
                        {
                            Err(_)            =>
                            {
                                write_runtime_log(&runtime_logger, "[Telemetry Receiver] spawn_blocking panic");
                                continue;
                            }
                            Ok(Err(why)) =>
                            {
                                write_runtime_log(
                                    &runtime_logger,
                                    &format!("[Telemetry Receiver] Parse fail: {why}"),
                                );
                                continue;
                            }
                            Ok(Ok(m)) => m,
                        };

                        // lab 2 — reception drift: actual gap between consecutive packets.
                        // Mixed-sensor stream (thermal 100ms, accel 50ms, gyro 20ms) means
                        // no single expected period applies — we log the raw inter-packet gap.
                        let drift_timing: i64 = if let Some(prev) = last_any_rx
                        {
                            prev.elapsed().as_millis() as i64
                        }
                        else { 0 };
                        last_any_rx = Some(Instant::now());

                        // ── serde: match on typed OcsMessage variants ─────────────────────
                        // Before serde this was: if payload.contains("\"tag\":\"alert\"") { ... }
                        // Now the compiler verifies every field exists and has the right type.
                        match &msg
                        {
                            OcsMessage::Alert { event, .. } =>
                            {
                                handle_ocs_alert(
                                    &msg,
                                    &state,
                                    &metrics,
                                    &cmd_tx,
                                    &runtime_logger,
                                    &simulation_start,
                                );
                                continue;
                            }

                            OcsMessage::Thermal { seq, temp, drift_timing: sensor_drift } =>
                            {
                                write_runtime_log(
                                    &runtime_logger,
                                    &format!(
                                        "[{}ms] [Telemetry Receiver] thermal  seq={seq}  \
                                         temp={temp:.2}°C  sensor_drift={sensor_drift:+}ms  decode={decode_time}µs",
                                        simulation_elapsed(&simulation_start)
                                    ),
                                );

                                // Lab 7: reset consecutive miss counter on success
                                let mut s = state.lock().unwrap();
                                s.thermal_misses = 0;
                                s.last_thermal   = simulation_elapsed(&simulation_start);

                                if s.loss_of_contact
                                {
                                    s.loss_of_contact = false;
                                    print_and_log(&runtime_logger, &format!(
                                        "[{}ms] [Telemetry Receiver] Thermal contact restored.",
                                        simulation_elapsed(&simulation_start)
                                    ));
                                }
                            }

                            OcsMessage::Status { iter, fill, state: sys_state, drift_timing: _ } =>
                            {
                                write_runtime_log(
                                    &runtime_logger,
                                    &format!(
                                        "[{}ms] [Telemetry Receiver] status   iter={iter}  \
                                         fill={fill:.1}%  state={sys_state}  decode={decode_time}µs",
                                        simulation_elapsed(&simulation_start)
                                    ),
                                );
                                let mut s = state.lock().unwrap();
                                s.thermal_misses = 0;
                                s.last_thermal   = simulation_elapsed(&simulation_start);
                                if s.loss_of_contact { s.loss_of_contact = false; }
                            }

                            OcsMessage::Accel { seq, mag } =>
                            {
                                write_runtime_log(
                                    &runtime_logger,
                                    &format!(
                                        "[{}ms] [Telemetry Receiver] accel    seq={seq}  \
                                         mag={mag:.4}  decode={decode_time}µs",
                                        simulation_elapsed(&simulation_start)
                                    ),
                                );
                                let mut s = state.lock().unwrap();
                                s.accelerometer_misses  = 0; // variable name changed
                                s.last_accelerometer    = simulation_elapsed(&simulation_start); // variable name changed
                                if s.loss_of_contact { s.loss_of_contact = false; }
                            }

                            OcsMessage::Gyro { generation, seq, omega_z } =>
                            {
                                write_runtime_log(
                                    &runtime_logger,
                                    &format!(
                                        "[{}ms] [Telemetry Receiver] gyro     gen={generation}  \
                                         seq={seq}  ω_z={omega_z:.4}  decode={decode_time}µs",
                                        simulation_elapsed(&simulation_start)
                                    ),
                                );
                                let mut s = state.lock().unwrap();
                                s.gyroscope_misses  = 0; // variable name changed
                                s.last_gyroscope    = simulation_elapsed(&simulation_start); // variable name changed
                                if s.loss_of_contact { s.loss_of_contact = false; }
                            }

                            OcsMessage::Downlink { pkt, bytes, queue_latency } =>
                            {
                                write_runtime_log(
                                    &runtime_logger,
                                    &format!(
                                        "[{}ms] [Telemetry Receiver] downlink pkt={pkt}  \
                                         {bytes}B  q_lat={queue_latency}ms  decode={decode_time}µs",
                                        simulation_elapsed(&simulation_start)
                                    ),
                                );
                                let mut s = state.lock().unwrap();
                                s.accelerometer_misses  = 0; // variable name changed
                                s.last_accelerometer    = simulation_elapsed(&simulation_start); // variable name changed
                                if s.loss_of_contact { s.loss_of_contact = false; }
                            }
                        }

                        let mut m = metrics.lock().unwrap();
                        m.telemetry_received += 1;
                        m.decode_latency.push(decode_time);
                        m.reception_drift_timing.push(drift_timing);
                    }
                }
            }
        }
    }
}

// Handle an incoming OCS Alert message.
// Takes the typed OcsMessage (already validated by serde) instead of a raw &str.
// Lab 7: classify the event string, then match on the typed enum variant.
fn handle_ocs_alert(
    msg:              &OcsMessage,
    state:            &Shared<GcsState>,
    metrics:          &Shared<GcsMetrics>,
    cmd_tx:           &mpsc::Sender<UplinkCmd>,
    runtime_logger:   &Shared<File>,
    simulation_start: &Instant,
)
{
    // Extract the event string from the Alert variant
    let event = match msg
    {
        OcsMessage::Alert { event, .. } => event.as_str(),
        _                               => return,  // not an alert — ignore
    };

    // Lab 7: convert event string → typed OcsFaultKind enum
    let kind = classify_event(event);

    let fault_msg = format!(
        "[{}ms] [OCS-ALERT] {kind:?}  event=\"{event}\"",
        simulation_elapsed(simulation_start)
    );
    print_and_log(runtime_logger, &fault_msg);

    // Lock each mutex briefly and release before the next — (lab 2 deadlock prevention)
    metrics.lock().unwrap().faults_received += 1;
    state.lock().unwrap().fault_log.push(fault_msg);

    // Lab 7: match on enum variant (mirrors the fault_injector match in OCS)
    match kind
    {
        OcsFaultKind::MissionAbort =>
        {
            let alert = format!(
                "[{}ms] !!! CRITICAL GROUND ALERT !!! Mission Abort",
                simulation_elapsed(simulation_start)
            );
            print_and_log(runtime_logger, &alert);
            metrics.lock().unwrap().critical_alerts.push(alert);

            {
                let mut s = state.lock().unwrap();
                if !s.fault_active
                {
                    s.fault_active      = true;
                    s.fault_detected_at = Some(Instant::now());
                }
            }

            // Emergency halt bypasses typestate — this IS the fault-setter path
            let payload = encode_command(&GcsCommand
            {
                tag:        "cmd".into(),
                cmd:        "EmergencyHalt".into(),
                ts:         simulation_elapsed(simulation_start),
                priority:   Some(1),
                iter:       None,
                generation: None,
            });
            let _ = cmd_tx.try_send(UplinkCmd
            {
                payload,
                priority:   1,
                created_at: Instant::now(),
            });
        }

        OcsFaultKind::ThermalAlert | OcsFaultKind::FaultInjected | OcsFaultKind::GyroRestart =>
        {
            let mut s = state.lock().unwrap();
            if !s.fault_active
            {
                s.fault_active      = true;
                s.fault_detected_at = Some(Instant::now());
                print_and_log(runtime_logger, &format!(
                    "[{}ms] [Fault Manager] Safety interlock ENGAGED — commands blocked",
                    simulation_elapsed(simulation_start)
                ));
            }
        }

        OcsFaultKind::Unknown =>
        {
            print_and_log(runtime_logger, &format!(
                "[{}ms] [Fault Manager] Unknown alert \"{event}\" — logged, interlock not engaged",
                simulation_elapsed(simulation_start)
            ));
        }
    }
}


// =============================================================================
//  LOSS-OF-CONTACT MONITOR  (Lab 7 consecutive-miss + Lab 6 heartbeat)
//
//  This task runs like the heartbeat() from Lab 6 — it fires periodically
//  and checks whether telemetry has been received recently.
//
//  Lab 7: consecutive miss counters are reset on every received telemetry
//  message (same pattern as glitch_count reset in Lab 7's run_sensor_loop).
//  When misses exceed LOSS_OF_CONTACT_MISS_THRESHOLD → Loss of Contact.
// =============================================================================

async fn loss_of_contact_monitor_task(
    state:            Shared<GcsState>,
    metrics:          Shared<GcsMetrics>,
    cmd_tx:           mpsc::Sender<UplinkCmd>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,
)
{
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Loss of Contact Monitor] Started.  threshold={LOSS_OF_CONTACT_MISS_THRESHOLD}",
            simulation_elapsed(&simulation_start)
        ),
    );

    loop
    {
        // Lab 8: select between shutdown and the next check interval
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [Loss of Contact Monitor] Exit.", simulation_elapsed(&simulation_start)),
                );
                return;
            }
            _ = sleep(Duration::from_millis(REREQUEST_INTERVAL)) => {}
        }

        let ts = simulation_elapsed(&simulation_start);

        // Read current miss counts and last-heard timestamps
        let (th, accel, gyro, last_th, last_accel, last_gyro, loc); // variable name changed
        {
            let s      = state.lock().unwrap();
            th         = s.thermal_misses;
            accel      = s.accelerometer_misses; // variable name changed
            gyro       = s.gyroscope_misses;     // variable name changed
            last_th    = s.last_thermal;
            last_accel = s.last_accelerometer;   // variable name changed
            last_gyro  = s.last_gyroscope;       // variable name changed
            loc        = s.loss_of_contact;
        }

        // Increment miss counters for any sensor channel that has been silent
        {
            let mut s = state.lock().unwrap();
            if ts.saturating_sub(last_th)    > TELEMETRY_WATCHDOG { s.thermal_misses       += 1; }
            if ts.saturating_sub(last_accel) > TELEMETRY_WATCHDOG { s.accelerometer_misses += 1; } // variable name changed
            if ts.saturating_sub(last_gyro)  > TELEMETRY_WATCHDOG { s.gyroscope_misses     += 1; } // variable name changed
        }

        let max_m = th.max(accel).max(gyro); // variable name changed

        if max_m >= LOSS_OF_CONTACT_MISS_THRESHOLD && !loc
        {
            let alert = format!(
                "[{ts}ms] !!! LOSS OF CONTACT !!!  thermal={th} accel={accel} gyro={gyro}" // variable name changed
            );
            print_and_log(&runtime_logger, &alert);
            metrics.lock().unwrap().critical_alerts.push(alert);
            state.lock().unwrap().loss_of_contact = true;
            metrics.lock().unwrap().missed_packets += max_m as u64;

            // serde: encode a re-request command and send to OCS
            let payload = encode_command(&GcsCommand
            {
                tag:        "cmd".into(),
                cmd:        "RetransmitRequest".into(),
                ts,
                priority:   Some(1),
                iter:       None,
                generation: None,
            });
            let _ = cmd_tx.try_send(UplinkCmd
            {
                payload,
                priority:   1,
                created_at: Instant::now(),
            });

            print_and_log(
                &runtime_logger,
                &format!("[{ts}ms] [Loss of Contact Monitor] Re-request sent to OCS."),
            );
        }
        else if loc && max_m < LOSS_OF_CONTACT_MISS_THRESHOLD
        {
            state.lock().unwrap().loss_of_contact = false;
            print_and_log(
                &runtime_logger,
                &format!("[{ts}ms] [Loss of Contact Monitor] Contact restored."),
            );
        }
    }
}


// =============================================================================
//  FAULT MANAGER  (Lab 3: interlock latency timing)
//
//  Monitors the fault_active flag and measures how long it takes from
//  fault detection to interlock engagement (Lab 3 latency measurement).
//  If the interlock takes longer than FAULT_RESPONSE_LIMIT → critical alert.
// =============================================================================

async fn fault_manager_task(
    state:            Shared<GcsState>,
    metrics:          Shared<GcsMetrics>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,
)
{
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Fault Manager] Started.  response_limit={}ms",
            simulation_elapsed(&simulation_start),
            FAULT_RESPONSE_LIMIT
        ),
    );

    loop
    {
        // Lab 8: select between shutdown and polling interval
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [Fault Manager] Exit.", simulation_elapsed(&simulation_start)),
                );
                return;
            }
            _ = sleep(Duration::from_millis(50)) => {}
        }

        // Only act if a fault is currently active
        if !state.lock().unwrap().fault_active { continue; }  // lock acquired and DROPPED here

        // Extract what we need in a scoped block so the lock is released immediately
        // (lab 2 — always drop Mutex locks before doing more work)
        let fault_detected_at = {
            state.lock().unwrap().fault_detected_at  // lock acquired and DROPPED at end of block
        };

        if let Some(t0) = fault_detected_at  // no lock held here at all
        {
            let interlock_ms = t0.elapsed().as_millis() as u64;

            // Lock metrics briefly, then release (lab 2)
            metrics.lock().unwrap().interlock_latency.push(interlock_ms);

            if interlock_ms > FAULT_RESPONSE_LIMIT
            {
                let alert = format!(
                    "[{}ms] !!! CRITICAL ALERT !!! Interlock {interlock_ms}ms > {FAULT_RESPONSE_LIMIT}ms",
                    simulation_elapsed(&simulation_start)
                );
                print_and_log(&runtime_logger, &alert);
                metrics.lock().unwrap().critical_alerts.push(alert);

                // Lock state to write — safe now because NO other lock is held (lab 2)
                {
                    let mut s = state.lock().unwrap();
                    s.fault_active      = false;
                    s.fault_detected_at = None;
                }
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [Fault Manager] Interlock auto-cleared.", simulation_elapsed(&simulation_start)),
                );
            }
        }
    }
}


// =============================================================================
//  PART 2 — COMMAND UPLINK TASKS  (Rate Monotonic + Lab 7 typestate)
//
//  Three tasks with different periods following Rate Monotonic scheduling.
//  Shorter period = higher priority = runs more often.
//
//  Each task follows this pattern:
//    1. tokio::select! { shutdown | sleep(period) }                   (Lab 8)
//    2. Check fault_active — reject if faulted                        (Lab 7)
//    3. Construct GcsMode::<Normal>::new() only when OK               (Lab 7)
//    4. Call dispatch_command(&mode, ...) — compile enforced          (Lab 7)
//    5. Encode command with serde_json::to_string(&GcsCommand{..})    (serde)
//    6. Measure and record jitter using .as_micros()                  (Lab 2)
// =============================================================================

// ── Thermal Check (RM P1) — fires every 50ms ─────────────────────────────────
async fn thermal_command_task(
    cmd_tx:           mpsc::Sender<UplinkCmd>,
    state:            Shared<GcsState>,
    metrics:          Shared<GcsMetrics>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,
)
{
    // This is the heartbeat() pattern from Lab 6 — a periodic async task
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Thermal Command] Started.  period={}ms  RM-P1",
            simulation_elapsed(&simulation_start),
            THERMAL_COMMAND_PERIOD
        ),
    );

    let mut iter:      u64     = 0;
    let task_start:    Instant = Instant::now();  // lab 3 — reference for drift
    let mut last_tick: Instant = Instant::now();  // lab 2 — previous tick for jitter

    loop
    {
        let t0 = Instant::now();

        // Lab 8: wait for either shutdown OR the period timer
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [Thermal Command] Exit.", simulation_elapsed(&simulation_start)),
                );
                return;
            }
            _ = sleep(Duration::from_millis(THERMAL_COMMAND_PERIOD)) => {}
        }

        // ── Drift (Lab 2) — how far behind/ahead of the ideal schedule ────────
        let expected_time  = iter * THERMAL_COMMAND_PERIOD;
        let actual_time    = task_start.elapsed().as_millis() as u64;
        let drift_timing = actual_time as i64 - expected_time as i64;

        // ── Jitter (Lab 2) — variation between consecutive intervals ──────────
        // .as_micros() used here: jitter is a short duration where microsecond
        // precision is meaningful. Milliseconds would lose too much resolution.
        let this_interval = last_tick.elapsed().as_micros() as i64;
        let jitter_timing = (this_interval - (THERMAL_COMMAND_PERIOD as i64 * 1000)).unsigned_abs() as i64;
        last_tick = Instant::now();

        if state.lock().unwrap().fault_active
        {
            let rej = format!(
                "[{}ms] [REJECT] ThermalCheck iter={iter} — FaultLocked",
                simulation_elapsed(&simulation_start)
            );
            print_and_log(&runtime_logger, &rej);
            let mut m = metrics.lock().unwrap();
            m.commands_rejected += 1;
            m.rejection_log.push(rej);
            iter += 1;
            continue;  // skip dispatch_command entirely
        }

        // Construct Normal mode ONLY when fault_active is false.
        // The type system makes it impossible to pass this to dispatch_command
        // if we were in the fault branch above.
        let mode = GcsMode::<Normal>::new();

        // serde: build a typed command struct, no manual JSON string escaping
        let payload = encode_command(&GcsCommand
        {
            tag:        "cmd".into(),
            cmd:        "ThermalCheck".into(),
            ts:         simulation_elapsed(&simulation_start),
            priority:   Some(1),
            iter:       Some(iter),
            generation: None,
        });

        // Lab 7: dispatch_command takes &GcsMode<Normal> — compile-time safety
        dispatch_command(&mode, &cmd_tx, payload, 1);

        if jitter_timing > UPLINK_JITTER_LIMIT
        {
            let warning = format!(
                "[{}ms] [WARN] Thermal Command jitter {}µs iter={iter}",
                simulation_elapsed(&simulation_start),
                jitter_timing
            );
            print_and_log(&runtime_logger, &warning);
            metrics.lock().unwrap().deadline_violations.push(warning);
        }

        {
            let mut m = metrics.lock().unwrap();
            m.thermal_jitter.push(jitter_timing);
            m.drift_timing.push(drift_timing);
            m.active_ms  += t0.elapsed().as_millis() as u64;
            m.elapsed_time  = task_start.elapsed().as_millis() as u64;
        }

        if iter % 20 == 0
        {
            write_runtime_log(
                &runtime_logger,
                &format!(
                    "[{}ms] [Thermal Command] iter={iter:>4}  drift={drift_timing:+}ms  jitter={jitter_timing}µs",
                    simulation_elapsed(&simulation_start)
                ),
            );
        }

        iter += 1;
    }
}

// ── Accelerometer Check (RM P2) — fires every 120ms ─────────────────────────── // variable name changed
async fn accelerometer_command_task( // variable name changed
    cmd_tx:           mpsc::Sender<UplinkCmd>,
    state:            Shared<GcsState>,
    metrics:          Shared<GcsMetrics>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,
)
{
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Accelerometer Command] Started.  period={}ms  RM-P2", // variable name changed
            simulation_elapsed(&simulation_start),
            ACCELEROMETER_COMMAND_PERIOD
        ),
    );

    let mut iter:      u64     = 0;
    let task_start:    Instant = Instant::now();
    let mut last_tick: Instant = Instant::now();

    loop
    {
        let t0 = Instant::now();

        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [Accelerometer Command] Exit.", simulation_elapsed(&simulation_start)), // variable name changed
                );
                return;
            }
            _ = sleep(Duration::from_millis(ACCELEROMETER_COMMAND_PERIOD)) => {}
        }

        let expected_time  = iter * ACCELEROMETER_COMMAND_PERIOD;
        let actual_time    = task_start.elapsed().as_millis() as u64;
        let drift_timing = actual_time as i64 - expected_time as i64;

        // .as_micros() for jitter — short duration, microsecond precision needed
        let this_interval = last_tick.elapsed().as_micros() as i64;
        let jitter_timing = (this_interval - (ACCELEROMETER_COMMAND_PERIOD as i64 * 1000)).unsigned_abs() as i64;
        last_tick = Instant::now();

        // Lab 7: reject if faulted
        if state.lock().unwrap().fault_active
        {
            let rej = format!(
                "[{}ms] [REJECT] AccelerometerCheck iter={iter} — FaultLocked", // variable name changed
                simulation_elapsed(&simulation_start)
            );
            print_and_log(&runtime_logger, &rej);
            let mut m = metrics.lock().unwrap();
            m.commands_rejected += 1;
            m.rejection_log.push(rej);
            iter += 1;
            continue;
        }

        let mode    = GcsMode::<Normal>::new();
        let payload = encode_command(&GcsCommand
        {
            tag:        "cmd".into(),
            cmd:        "AccelerometerCheck".into(), // variable name changed
            ts:         simulation_elapsed(&simulation_start),
            priority:   Some(2),
            iter:       Some(iter),
            generation: None,
        });

        dispatch_command(&mode, &cmd_tx, payload, 2);

        {
            let mut m = metrics.lock().unwrap();
            m.accelerometer_jitter.push(jitter_timing); // variable name changed
            m.drift_timing.push(drift_timing);
            m.active_ms += t0.elapsed().as_millis() as u64;
        }

        if iter % 8 == 0
        {
            write_runtime_log(
                &runtime_logger,
                &format!(
                    "[{}ms] [Accelerometer Command] iter={iter:>4}  drift={drift_timing:+}ms  jitter={jitter_timing}µs", // variable name changed
                    simulation_elapsed(&simulation_start)
                ),
            );
        }

        iter += 1;
    }
}


// =============================================================================
//  LAB 8 PART 1 — FRAGILE GYROSCOPE DISPATCHER  // variable name changed
//
//  This is the fragile_worker pattern from Lab 8 applied to an async task.
//  It has a 2% random chance of panicking each iteration, simulating a
//  transmitter hardware glitch.
//
//  Like fragile_gyroscope in OCS, it runs in an infinite loop — the
//  supervisor detects the panic and restarts it.
// =============================================================================

async fn fragile_gyroscope_dispatcher( // variable name changed
    generation:       u32,
    cmd_tx:           mpsc::Sender<UplinkCmd>,
    state:            Shared<GcsState>,
    metrics:          Shared<GcsMetrics>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
)
{
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Gyroscope Command-{generation}] Started.  period={}ms  RM-P3  fragile", // variable name changed
            simulation_elapsed(&simulation_start),
            GYROSCOPE_COMMAND_PERIOD
        ),
    );

    let mut iter:      u64     = 0;
    let task_start:    Instant = Instant::now();
    let mut last_tick: Instant = Instant::now();

    loop
    {
        // Lab 6: async sleep — yields execution so other tasks can run
        sleep(Duration::from_millis(GYROSCOPE_COMMAND_PERIOD)).await;

        // Lab 8: 2% random panic (fragile_worker pattern from Lab 8)
        if rand::random_range(0u32..100) < 2
        {
            print_and_log(
                &runtime_logger,
                &format!(
                    "[{}ms] [Gyroscope Command-{generation}] Transmitter glitch — panicking!", // variable name changed
                    simulation_elapsed(&simulation_start)
                ),
            );
            panic!("Gyroscope dispatcher fault (gen {generation})"); // variable name changed
            // gyroscope_supervisor_task catches this via handle.await returning Err
        }

        let expected_time  = iter * GYROSCOPE_COMMAND_PERIOD;
        let actual_time    = task_start.elapsed().as_millis() as u64;
        let drift_timing = actual_time as i64 - expected_time as i64;

        // .as_micros() for jitter — short duration, microsecond precision needed
        let this_interval = last_tick.elapsed().as_micros() as i64;
        let jitter_timing = (this_interval - (GYROSCOPE_COMMAND_PERIOD as i64 * 1000)).unsigned_abs() as i64;
        last_tick = Instant::now();

        // Lab 7: reject if faulted
        if state.lock().unwrap().fault_active
        {
            let rej = format!(
                "[{}ms] [REJECT] GyroscopeCheck gen={generation} iter={iter}", // variable name changed
                simulation_elapsed(&simulation_start)
            );
            print_and_log(&runtime_logger, &rej);
            let mut m = metrics.lock().unwrap();
            m.commands_rejected += 1;
            m.rejection_log.push(rej);
            iter += 1;
            continue;
        }

        let mode    = GcsMode::<Normal>::new();
        let payload = encode_command(&GcsCommand
        {
            tag:        "cmd".into(),
            cmd:        "GyroscopeCheck".into(), // variable name changed
            ts:         simulation_elapsed(&simulation_start),
            priority:   Some(3),
            iter:       Some(iter),
            generation: Some(generation),
        });

        dispatch_command(&mode, &cmd_tx, payload, 3);

        {
            let mut m = metrics.lock().unwrap();
            m.gyroscope_jitter.push(jitter_timing); // variable name changed
            m.drift_timing.push(drift_timing);
        }

        if iter % 3 == 0
        {
            write_runtime_log(
                &runtime_logger,
                &format!(
                    "[{}ms] [Gyroscope Command-{generation}] iter={iter:>4}  drift={drift_timing:+}ms  jitter={jitter_timing}µs", // variable name changed
                    simulation_elapsed(&simulation_start)
                ),
            );
        }

        iter += 1;
    }
}


// =============================================================================
//  LAB 8 PART 2 — GYROSCOPE SUPERVISOR  (async version of Lab 8 supervisor) // variable name changed
//
//  Mirrors the gyro_supervisor_thread in OCS, but runs as a Tokio task.
//  Uses tokio::spawn instead of thread::spawn, and .await instead of .join().
//
//  Lab 8 match pattern:
//    match handle.await {
//        Ok(_)              => normal exit
//        Err(e) if e.is_panic() => restart after backoff
//        Err(_)             => cancelled
//    }
// =============================================================================

async fn gyroscope_supervisor_task( // variable name changed
    cmd_tx:           mpsc::Sender<UplinkCmd>,
    state:            Shared<GcsState>,
    metrics:          Shared<GcsMetrics>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,
)
{
    print_and_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Gyroscope Supervisor] Supervisor online.",
            simulation_elapsed(&simulation_start)
        ),
    );

    let mut generation: u32 = 0;

    loop
    {
        // Check if we should exit before spawning a new generation
        if shutdown.is_cancelled()
        {
            print_and_log(
                &runtime_logger,
                &format!("[{}ms] [Gyroscope Supervisor] Exit.", simulation_elapsed(&simulation_start)),
            );
            return;
        }

        generation += 1;
        print_and_log(
            &runtime_logger,
            &format!(
                "[{}ms] [Gyroscope Supervisor] Starting generation {generation}...",
                simulation_elapsed(&simulation_start)
            ),
        );

        // Lab 8: spawn the fragile dispatcher as a new async task
        let handle = tokio::spawn(fragile_gyroscope_dispatcher( // variable name changed
            generation,
            cmd_tx.clone(),
            Arc::clone(&state),
            Arc::clone(&metrics),
            Arc::clone(&runtime_logger),
            simulation_start,
        ));

        // Lab 8: match handle.await to detect panic vs normal exit
        match handle.await
        {
            Ok(_) =>
            {
                // Normal exit — shouldn't happen in loop{}, means it was cancelled
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [Gyroscope Supervisor] Normal exit — done.", simulation_elapsed(&simulation_start)),
                );
                return;
            }
            Err(e) if e.is_panic() =>
            {
                // Panic detected — same restart logic as Lab 8
                metrics.lock().unwrap().gyroscope_restarts += 1; // variable name changed
                print_and_log(
                    &runtime_logger,
                    &format!(
                        "[{}ms] [Gyroscope Supervisor] Panic detected! Restarting in 1s...",
                        simulation_elapsed(&simulation_start)
                    ),
                );

                // Lab 8: backoff before restarting
                sleep(Duration::from_secs(1)).await;
            }
            Err(_) =>
            {
                // Cancelled — clean exit
                print_and_log(
                    &runtime_logger,
                    &format!("[{}ms] [Gyroscope Supervisor] Cancelled.", simulation_elapsed(&simulation_start)),
                );
                return;
            }
        }
    }
}


// =============================================================================
//  METRICS REPORTER
//
//  Prints a formatted summary report every 10 seconds.
//  Lab 6: runs as a tokio::spawn'd async task alongside all others.
// =============================================================================

async fn metrics_reporter_task(
    metrics:          Shared<GcsMetrics>,
    state:            Shared<GcsState>,
    backlog_counter:  Shared<usize>,
    runtime_logger:   Shared<File>,
    simulation_start: Instant,
    shutdown:         CancellationToken,
)
{
    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() => return,
            _ = sleep(Duration::from_secs(10)) => {}
        }

        // lab 2 — snapshot the current backlog depth into metrics
        let depth = *backlog_counter.lock().unwrap();
        {
            let mut m = metrics.lock().unwrap();
            m.backlog_depth_samples.push(depth);
            if depth > m.backlog_peak { m.backlog_peak = depth; }
        }

        print_report(
            &metrics.lock().unwrap(),
            &state.lock().unwrap(),
            &simulation_start,
        );
    }
}

fn print_report(m: &GcsMetrics, s: &GcsState, simulation_start: &Instant)
{
    let elapsed_time = simulation_start.elapsed().as_millis();

    let reject_pct = if m.commands_sent + m.commands_rejected > 0
    {
        m.commands_rejected as f64
            / (m.commands_sent + m.commands_rejected) as f64
            * 100.0
    }
    else { 0.0 };

    let cpu = if m.elapsed_time > 0
    {
        m.active_ms as f64 / m.elapsed_time as f64 * 100.0
    }
    else { 0.0 };

    let avg_depth = if m.backlog_depth_samples.is_empty() { 0.0 }
    else
    {
        m.backlog_depth_samples.iter().sum::<usize>() as f64
            / m.backlog_depth_samples.len() as f64
    };

    println!("\n╔══════════════════════════════════════════════════════╗");
    println!( "║  GCS REPORT  at {elapsed_time}ms simulation time");
    println!( "╠══════════════════════════════════════════════════════╣");
    println!(
        "║  Mode: {}  LoC: {}  Gyroscope restarts: {}", // variable name changed
        if s.fault_active { "FaultLocked" } else { "Normal" },
        s.loss_of_contact,
        m.gyroscope_restarts // variable name changed
    );
    println!(
        "║  Telemetry: rx={}  missed={}",
        m.telemetry_received, m.missed_packets
    );
    println!(
        "║  Backlog depth  avg={:.1}  peak={}  channel_cap=100",
        avg_depth, m.backlog_peak
    );
    println!(
        "║  Decode latency (µs)  deadline={}ms  (spawn_blocking)",
        DECODE_DEADLINE
    );
    let dl: Vec<i64> = m.decode_latency.iter().map(|&v| v as i64).collect();
    print_stat_row("Decode (µs)", &dl);
    println!(
        "║  Commands  sent={}  rejected={} ({:.1}%)",
        m.commands_sent, m.commands_rejected, reject_pct
    );
    println!(
        "║  Dispatch latency (µs)  deadline={}ms",
        DISPATCH_DEADLINE
    );
    let disp: Vec<i64> = m.dispatch_latency.iter().map(|&v| v as i64).collect();
    print_stat_row("Dispatch (µs)", &disp);
    println!("║  Uplink jitter (µs)  limit={}µs", UPLINK_JITTER_LIMIT);
    print_stat_row("ThermalCheck       RM-P1", &m.thermal_jitter);
    print_stat_row("AccelerometerCheck RM-P2", &m.accelerometer_jitter); // variable name changed
    print_stat_row("GyroscopeCheck     RM-P3", &m.gyroscope_jitter);     // variable name changed
    println!("║  Drift (ms)");
    print_stat_row("All tasks", &m.drift_timing);
    println!(
        "║  Faults: {}  Critical alerts: {}",
        m.faults_received, m.critical_alerts.len()
    );
    if !m.interlock_latency.is_empty()
    {
        let il: Vec<i64> = m.interlock_latency.iter().map(|&v| v as i64).collect();
        print_stat_row("Interlock latency (ms)", &il);
    }
    println!("║  Deadline violations: {}", m.deadline_violations.len());
    for v in m.deadline_violations.iter().take(3)
    {
        println!("║    {v}");
    }
    if m.deadline_violations.len() > 3
    {
        println!("║    ... and {} more", m.deadline_violations.len() - 3);
    }
    if !m.rejection_log.is_empty()
    {
        println!("║  Rejections: {}", m.rejection_log.len());
        for r in m.rejection_log.iter().take(2)
        {
            println!("║    {r}");
        }
    }
    println!("║  CPU ≈ {cpu:.2}%");
    if !m.critical_alerts.is_empty()
    {
        println!("║  CRITICAL ALERTS:");
        for a in &m.critical_alerts
        {
            println!("║    {a}");
        }
    }
    println!("╚══════════════════════════════════════════════════════╝\n");
}


// =============================================================================
//  MAIN FUNCTION   (Lab 6: #[tokio::main] async entry point)
//
//  #[tokio::main] wraps main() in a Tokio async runtime.
//  All tasks are spawned with tokio::spawn (Lab 6) and share state
//  via Arc<Mutex<T>> (Lab 2).
//  Graceful shutdown is coordinated via CancellationToken (Lab 8).
// =============================================================================

#[tokio::main]
async fn main()
{
    println!("╔═══════════════════════════════════════════════════════╗");
    println!("║  GCS — Ground Control Station                         ║");
    println!("║  CT087-3-3  |  Student B  |  Soft RTS / Tokio         ║");
    println!("╚═══════════════════════════════════════════════════════╝\n");

    // ── Simulation start time ─────────────────────────────────────────────────
    // All timestamps throughout the GCS are simulation-relative.
    // simulation_elapsed(&simulation_start) gives ms since this moment.
    // This matches OCS which also uses simulation-relative time.
    let simulation_start = Instant::now();

    // ── Runtime log file ──────────────────────────────────────────────────────
    // High-frequency per-packet events go to the log file, not the terminal.
    // Important events (faults, alerts, violations) go to both via print_and_log().
    let runtime_logger = create_runtime_logger();
    println!("[GCS] Runtime log → {RUNTIME_LOG_FILE}");

    // ── Shared State Setup ────────────────────────────────────────────────────
    // Lab 2: Arc<Mutex<T>> — shared ownership with mutual exclusion
    let state    = Arc::new(Mutex::new(GcsState::default()));
    let metrics  = Arc::new(Mutex::new(GcsMetrics::default()));

    // lab 2 — Arc<Mutex<T>> backlog depth counter
    // incremented by udp_receiver_loop on successful channel send
    // decremented by telemetry_processor_task on each packet consumed
    let backlog_counter: Shared<usize> = Arc::new(Mutex::new(0));

    // Lab 8: CancellationToken — clean shutdown signal sent to all tasks
    let shutdown = CancellationToken::new();

    // ── Lab 9: Async UDP Sockets ──────────────────────────────────────────────
    // tokio::net::UdpSocket is the async version of the std::net::UdpSocket
    // used in Lab 9's blocking server code.
    let recv_sock = Arc::new(
        UdpSocket::bind(GCS_TELEMETRY_BIND)
            .await
            .expect("[GCS] bind recv failed")
    );
    let send_sock = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("[GCS] bind send failed")
    );

    // ── Lab 11: Two mpsc channels ─────────────────────────────────────────────
    // incoming: udp_receiver_loop → telemetry_processor_task
    // cmd_out:  command tasks    → udp_sender_task
    let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingPacket>(100);
    let (cmd_tx,       cmd_rx)     = mpsc::channel::<UplinkCmd>(50);

    println!("[GCS] Config:");
    println!("      Receive          : {GCS_TELEMETRY_BIND}  (serde_json decoded)");
    println!("      Send to          : {OCS_COMMAND_ADDRESS}  (serde_json encoded)");
    println!("      Typestate        : GcsMode<Normal/FaultLocked>  (Lab 7 PhantomData)");
    println!("      Decode           : spawn_blocking  (Lab 6 CPU-off-async)");
    println!(
        "      RM periods       : thermal={}ms  accel={}ms  gyro={}ms", // variable name changed
        THERMAL_COMMAND_PERIOD, ACCELEROMETER_COMMAND_PERIOD, GYROSCOPE_COMMAND_PERIOD
    );
    println!("      Sim duration     : {SIMULATION_DURATION}s\n");

    // ── Lab 6: tokio::spawn for each subsystem ────────────────────────────────
    // Each spawn creates a lightweight async task (NOT an OS thread).
    // Tasks cooperatively yield when they call .await, allowing others to run.

    // UDP receiver — listens for OCS telemetry, feeds the incoming channel
    tokio::spawn(udp_receiver_loop(
        Arc::clone(&recv_sock),
        incoming_tx,
        Arc::clone(&backlog_counter),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    // UDP sender — consumes command queue, fires packets to OCS
    tokio::spawn(udp_sender_task(
        cmd_rx,
        Arc::clone(&send_sock),
        Arc::clone(&metrics),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    // Telemetry processor — decodes packets (spawn_blocking) and routes them
    tokio::spawn(telemetry_processor_task(
        incoming_rx,
        Arc::clone(&state),
        Arc::clone(&metrics),
        cmd_tx.clone(),
        Arc::clone(&backlog_counter),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    // Loss-of-contact monitor — watchdog for telemetry silence
    tokio::spawn(loss_of_contact_monitor_task(
        Arc::clone(&state),
        Arc::clone(&metrics),
        cmd_tx.clone(),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    // Fault manager — measures interlock response latency
    tokio::spawn(fault_manager_task(
        Arc::clone(&state),
        Arc::clone(&metrics),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    // RM command tasks (Lab 7 typestate + Lab 8 supervisor)
    tokio::spawn(thermal_command_task(
        cmd_tx.clone(),
        Arc::clone(&state),
        Arc::clone(&metrics),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    tokio::spawn(accelerometer_command_task( // variable name changed
        cmd_tx.clone(),
        Arc::clone(&state),
        Arc::clone(&metrics),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    // Gyroscope supervisor wraps the fragile dispatcher (Lab 8)
    tokio::spawn(gyroscope_supervisor_task( // variable name changed
        cmd_tx.clone(),
        Arc::clone(&state),
        Arc::clone(&metrics),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    // Periodic metrics reporter
    tokio::spawn(metrics_reporter_task(
        Arc::clone(&metrics),
        Arc::clone(&state),
        Arc::clone(&backlog_counter),
        Arc::clone(&runtime_logger),
        simulation_start,
        shutdown.clone(),
    ));

    println!("[GCS] All tasks online.  Waiting for OCS telemetry...\n");

    // ── Run for the simulation duration ──────────────────────────────────────
    sleep(Duration::from_secs(SIMULATION_DURATION)).await;

    // ── Lab 8: Graceful Shutdown via CancellationToken ────────────────────────
    // Calling shutdown.cancel() wakes every tokio::select! branch that is
    // waiting on shutdown.cancelled() — all tasks exit cleanly.
    let total_time = simulation_start.elapsed().as_millis();
    println!("\n[GCS] Simulation ended at {total_time}ms — shutting down...");
    shutdown.cancel();

    // Give tasks a moment to finish cleanup before printing the final report
    sleep(Duration::from_millis(500)).await;

    // Print final summary
    println!("\n[GCS] FINAL REPORT:");
    print_report(&metrics.lock().unwrap(), &state.lock().unwrap(), &simulation_start);
    println!("[GCS] Done.  Full event log → {RUNTIME_LOG_FILE}");
}