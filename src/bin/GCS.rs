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
//  Incoming OCS payloads are deserialised with serde_json::from_str::<OcsMsg>().
//  Outgoing GCS commands are serialised with serde_json::to_string(&GcsCmd{}).
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
use std::marker::PhantomData;               // for typestate pattern (Lab 7)
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};               // for shared ownership across tasks (Lab 2)
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
// Shorter period = higher RM priority (Lab 11 concept)
const THERMAL_CMD_PERIOD_MS:  u64 = 50;    // RM P1 — fastest, highest priority
const VELOCITY_CMD_PERIOD_MS: u64 = 120;   // RM P2
const ATTITUDE_CMD_PERIOD_MS: u64 = 333;   // RM P3 — slowest, lowest priority

// ── Timing Deadlines ─────────────────────────────────────────────────────────
const DECODE_DEADLINE_MS:      u64 = 3;    // soft deadline: parse must complete within 3ms
const DISPATCH_DEADLINE_MS:    u64 = 2;    // soft deadline: command must be sent within 2ms
const FAULT_RESPONSE_LIMIT_MS: u64 = 100;  // interlock must engage within 100ms
const LOC_MISS_THRESHOLD:      u32 = 3;    // loss-of-contact after 3 watchdog misses
const UPLINK_JITTER_LIMIT_US:  i64 = 2_000;  // warn if uplink jitter > 2ms // TODO: 
const TELEM_WATCHDOG_MS:       u64 = 800;  // how long without telemetry before incrementing misses // TODO: 
const REREQUEST_INTERVAL_MS:   u64 = 500;  // how often the LoC monitor checks // TODO: 

// ── Simulation Config ─────────────────────────────────────────────────────────
const SIM_DURATION_S: u64 = 180;

// ── Network Addresses ─────────────────────────────────────────────────────────
const GCS_TELEM_BIND: &str = "0.0.0.0:9000";
const OCS_CMD_ADDR:   &str = "127.0.0.1:9001";
const STUDENT_ID:     &str = "tp071542";


// =============================================================================
//  SERDE MESSAGE TYPES  (shared wire format with OCS)
//
//  OcsMsg mirrors the enum in OCS.rs exactly.
//  serde_json::from_str::<OcsMsg>(&payload) replaces the old error-prone:
//    if payload.contains("\"tag\":\"alert\"") { ... }
//
//  GcsCmd is serialised with serde_json::to_string(&GcsCmd{...}) and
//  replaces every manual format!("{{\"tag\":\"cmd\",...}}") string.
// =============================================================================

// Incoming telemetry messages from the OCS — enum with one variant per message type
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "tag", rename_all = "snake_case")]
enum OcsMsg
{
    Thermal  { student: String, seq: u64, temp: f64,    drift_ms: i64 },
    Accel    { student: String, seq: u64, mag:  f64                    },
    Gyro     { student: String, generation: u32, seq:  u64, omega_z:    f64   },
    Status   { student: String, iter: u64, fill: f64, state: String, drift_ms: i64 },
    Downlink { student: String, pkt: u64, bytes: usize, q_lat_ms: u64  },

    // Alert uses #[serde(flatten)] so AlertInfo fields merge directly into
    // the JSON object (same as OCS.rs)
    Alert    { student: String, event: String, #[serde(flatten)] info: AlertInfo },
}

// Optional extra fields inside an Alert (same as OCS.rs)
#[derive(Serialize, Deserialize, Debug, Default)]
struct AlertInfo
{
    #[serde(skip_serializing_if = "Option::is_none")]
    pub misses:     Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub count:      Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault_type: Option<String>,
}

// Outgoing command sent to OCS over UDP
// Optional fields are omitted from JSON when None (cleaner wire format)
#[derive(Serialize, Deserialize, Debug)]
struct GcsCmd
{
    pub tag:     String,   // always "cmd"
    pub student: String,
    pub cmd:     String,   // e.g. "ThermalCheck", "EmergencyHalt"
    pub ts:      u64,      // Unix timestamp in ms

    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,   // RM priority of the issuing task

    #[serde(skip_serializing_if = "Option::is_none")]
    pub iter:     Option<u64>,  // which iteration of the task sent this

    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation:      Option<u32>,  // which generation of the attitude dispatcher
}

// Helper: serialise a GcsCmd to a JSON string.
// Logs on error and returns "" so the caller can safely send an empty string.
fn encode_cmd(cmd: &GcsCmd) -> String
{
    serde_json::to_string(cmd)
        .unwrap_or_else(|e|
        {
            println!("[ENCODE] {e}");
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
    decode_latency_us:    Vec<u64>,
    reception_drift_ms:   Vec<i64>,

    // Command uplink
    commands_sent:        u64,
    commands_rejected:    u64,
    dispatch_latency_us:  Vec<u64>,
    rejection_log:        Vec<String>,

    // Timing violations
    deadline_violations:  Vec<String>,

    // Lab 2 / Lab 11: jitter per RM task
    thermal_jitter_us:    Vec<i64>,
    velocity_jitter_us:   Vec<i64>,
    attitude_jitter_us:   Vec<i64>,

    // Fault handling
    faults_received:      u64,
    interlock_latency_ms: Vec<u64>,
    critical_alerts:      Vec<String>,

    // CPU estimate
    drift_ms:    Vec<i64>,
    active_ms:   u64,
    elapsed_ms:  u64,

    // Lab 8: how many times attitude supervisor restarted
    attitude_restarts: u32,
}

// Runtime GCS state shared across tasks
struct GcsState
{
    fault_active:       bool,
    fault_detected_at:  Option<Instant>,
    fault_log:          Vec<String>,

    // Lab 7: consecutive miss counters (same pattern as Lab 7's glitch_count)
    thermal_misses:     u32,
    velocity_misses:    u32,
    attitude_misses:    u32,

    // Watchdog timestamps — updated on each successful telemetry receipt
    last_thermal_ms:    u64,
    last_velocity_ms:   u64,
    last_attitude_ms:   u64,

    loss_of_contact:    bool,
}

// impl Default so we can write GcsState::default() in main
impl Default for GcsState
{
    fn default() -> Self
    {
        let t = now_ms();
        GcsState
        {
            fault_active:      false,
            fault_detected_at: None,
            fault_log:         Vec::new(),
            thermal_misses:    0,
            velocity_misses:   0,
            attitude_misses:   0,
            last_thermal_ms:   t,
            last_velocity_ms:  t,
            last_attitude_ms:  t,
            loss_of_contact:   false,
        }
    }
}


// =============================================================================
//  HELPER FUNCTIONS
// =============================================================================

// Returns current Unix time in milliseconds
fn now_ms() -> u64
{
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// Pretty-print one row of the metrics table (Lab 3: min/max/avg statistics)
fn print_stat_row(label: &str, samples: &[i64])
{
    if samples.is_empty()
    {
        println!("    {:<36} — no data", label);
        return;
    }

    let min = samples.iter().min().unwrap();
    let max = samples.iter().max().unwrap();
    let avg = samples.iter().sum::<i64>() as f64 / samples.len() as f64;

    println!(
        "    {:<36} n={:>5}  min={:>7}  max={:>7}  avg={:>9.1}",
        label, samples.len(), min, max, avg
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
    sock:        Arc<UdpSocket>,
    incoming_tx: mpsc::Sender<IncomingPacket>,  // Lab 11: producer side of channel
    shutdown:    CancellationToken,              // Lab 8: cancellation signal
)
{
    println!("[UDP-RX]   Listening for OCS telemetry on {GCS_TELEM_BIND}");

    let mut buf = vec![0u8; 65535];

    loop
    {
        // Lab 8: tokio::select! waits for the FIRST of these two futures to complete
        tokio::select!
        {
            // Branch 1: shutdown was requested
            _ = shutdown.cancelled() =>
            {
                println!("[UDP-RX]   Exit.");
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
                        if incoming_tx.try_send(IncomingPacket { payload, received_at, from }).is_err()
                        {
                            println!("[UDP-RX]   Channel full — packet dropped");
                        }
                    }
                    Err(e) => println!("[UDP-RX]   recv_from: {e}"),
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
    mut rx:   mpsc::Receiver<UplinkCmd>,
    sock:     Arc<UdpSocket>,
    metrics:  Arc<Mutex<GcsMetrics>>,
    shutdown: CancellationToken,
)
{
    println!("[UDP-TX]   Command sender → {OCS_CMD_ADDR}");

    loop
    {
        // Lab 8: select on either shutdown or next command
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                println!("[UDP-TX]   Exit.");
                return;
            }

            cmd = rx.recv() =>
            {
                match cmd
                {
                    Some(c) =>
                    {
                        // Lab 3: measure how long the command sat in the queue
                        let dispatch_us = c.created_at.elapsed().as_micros() as u64;

                        match sock.send_to(c.payload.as_bytes(), OCS_CMD_ADDR).await
                        {
                            Ok(_) =>
                            {
                                // Check dispatch deadline
                                if dispatch_us / 1_000 > DISPATCH_DEADLINE_MS
                                {
                                    let v = format!(
                                        "[{}ms] [DEADLINE] Dispatch {}µs > {}ms",
                                        now_ms(), dispatch_us, DISPATCH_DEADLINE_MS
                                    );
                                    println!("{v}");
                                    metrics.lock().unwrap().deadline_violations.push(v);
                                }

                                let mut m = metrics.lock().unwrap();
                                m.commands_sent += 1;
                                m.dispatch_latency_us.push(dispatch_us);
                            }
                            Err(e) => println!("[UDP-TX]   send_to: {e}"),
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
//  and decodes them into typed OcsMsg values using serde_json.
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
// serde_json::from_str parses the raw bytes into a typed OcsMsg.
fn decode_packet_sync(payload: String) -> Result<OcsMsg, String>
{
    serde_json::from_str::<OcsMsg>(&payload)
        .map_err(|e| format!("serde parse error: {e}  payload={payload}"))
}

async fn telemetry_processor_task(
    mut rx:   mpsc::Receiver<IncomingPacket>,   // Lab 11: consumer
    state:    Arc<Mutex<GcsState>>,
    metrics:  Arc<Mutex<GcsMetrics>>,
    cmd_tx:   mpsc::Sender<UplinkCmd>,
    shutdown: CancellationToken,
)
{
    println!(
        "[TelRx]    Processor ready.  decode_deadline={}ms  (spawn_blocking + serde)",
        DECODE_DEADLINE_MS
    );

    let mut last_any_rx: Option<Instant> = None;

    loop
    {
        // Lab 8: select on either shutdown or next incoming packet
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                println!("[TelRx]    Exit.");
                return;
            }

            pkt = rx.recv() =>
            {
                match pkt
                {
                    None => return,  // channel closed
                    Some(p) =>
                    {
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

                        // Lab 3: measure decode latency from the moment the packet arrived
                        let decode_us = arrival.elapsed().as_micros() as u64;
                        let decode_ms = decode_us / 1_000;

                        if decode_ms > DECODE_DEADLINE_MS
                        {
                            let v = format!(
                                "[{}ms] [DEADLINE] Decode {}ms > {}ms  from={from}",
                                now_ms(), decode_ms, DECODE_DEADLINE_MS
                            );
                            println!("{v}");
                            metrics.lock().unwrap().deadline_violations.push(v);
                        }

                        // Unwrap the JoinHandle result first, then the parse result
                        let msg = match decode_result
                        {
                            Err(_)       =>
                            {
                                println!("[TelRx]    spawn_blocking panic");
                                continue;
                            }
                            Ok(Err(why)) =>
                            {
                                println!("[TelRx]    Parse fail: {why}");
                                continue;
                            }
                            Ok(Ok(m))    => m,
                        };

                        // Reception drift (Lab 2: jitter formula)
                        let drift_ms: i64 = if let Some(prev) = last_any_rx //TODO ???? FORMULA WRONG?
                        {
                            prev.elapsed().as_millis() as i64
                                - THERMAL_CMD_PERIOD_MS as i64 * 10
                        }
                        else
                        {
                            0
                        };
                        last_any_rx = Some(Instant::now());

                        // ── serde: match on typed OcsMsg variants ────────────────────────
                        // Before serde, this was: if payload.contains("\"tag\":\"alert\"") { ... }
                        // Now the compiler verifies every field exists and has the right type.
                        match &msg
                        {
                            OcsMsg::Alert { event, .. } =>
                            {
                                // Delegate to the fault handler
                                handle_ocs_alert(&msg, &state, &metrics, &cmd_tx);
                                continue;
                            }

                            OcsMsg::Thermal { seq, temp, drift_ms: sensor_drift, .. } =>
                            {
                                println!(
                                    "[TelRx]    thermal  seq={seq}  temp={temp:.2}°C  \
                                     sensor_drift={sensor_drift:+}ms  decode={decode_us}µs"
                                );

                                // Lab 7: reset consecutive miss counter on success
                                let mut s = state.lock().unwrap();
                                s.thermal_misses  = 0;
                                s.last_thermal_ms = now_ms();

                                if s.loss_of_contact
                                {
                                    s.loss_of_contact = false;
                                    println!("[TelRx] Contact restored.");
                                }
                            }

                            OcsMsg::Status { iter, fill, state: sys_state, .. } =>
                            {
                                println!(
                                    "[TelRx]    status   iter={iter}  fill={fill:.1}%  \
                                     state={sys_state}  decode={decode_us}µs"
                                );
                                let mut s = state.lock().unwrap();
                                s.thermal_misses  = 0;
                                s.last_thermal_ms = now_ms();
                                if s.loss_of_contact { s.loss_of_contact = false; }
                            }

                            OcsMsg::Accel { seq, mag, .. } =>
                            {
                                println!(
                                    "[TelRx]    accel    seq={seq}  mag={mag:.4}  \
                                     decode={decode_us}µs"
                                );
                                let mut s = state.lock().unwrap();
                                s.velocity_misses  = 0;
                                s.last_velocity_ms = now_ms();
                                if s.loss_of_contact { s.loss_of_contact = false; }
                            }

                            OcsMsg::Gyro { generation, seq, omega_z, .. } =>
                            {
                                println!(
                                    "[TelRx]    gyro     gen={generation}  seq={seq}  \
                                     ω_z={omega_z:.4}  decode={decode_us}µs"
                                );
                                let mut s = state.lock().unwrap();
                                s.attitude_misses  = 0;
                                s.last_attitude_ms = now_ms();
                                if s.loss_of_contact { s.loss_of_contact = false; }
                            }

                            OcsMsg::Downlink { pkt, bytes, q_lat_ms, .. } =>
                            {
                                println!(
                                    "[TelRx]    downlink pkt={pkt}  {bytes}B  \
                                     q_lat={q_lat_ms}ms  decode={decode_us}µs"
                                );
                                let mut s = state.lock().unwrap();
                                s.velocity_misses  = 0;
                                s.last_velocity_ms = now_ms();
                                if s.loss_of_contact { s.loss_of_contact = false; }
                            }
                        }

                        let mut m = metrics.lock().unwrap();
                        m.telemetry_received += 1;
                        m.decode_latency_us.push(decode_us);
                        m.reception_drift_ms.push(drift_ms);
                    }
                }
            }
        }
    }
}

// Handle an incoming OCS Alert message.
// Takes the typed OcsMsg (already validated by serde) instead of a raw &str.
// Lab 7: classify the event string, then match on the typed enum variant.
fn handle_ocs_alert(
    msg:     &OcsMsg,
    state:   &Arc<Mutex<GcsState>>,
    metrics: &Arc<Mutex<GcsMetrics>>,
    cmd_tx:  &mpsc::Sender<UplinkCmd>,
)
{
    // Extract the event string from the Alert variant
    let event = match msg
    {
        OcsMsg::Alert { event, .. } => event.as_str(),
        _                           => return,  // not an alert — ignore
    };

    // Lab 7: convert event string → typed OcsFaultKind enum
    let kind = classify_event(event);
    println!("[FaultMgr] OCS alert: {kind:?}  event=\"{event}\"");

    let fault_msg = format!("[{}ms] [OCS-ALERT] {kind:?}", now_ms());

    let mut s = state.lock().unwrap();
    let mut m = metrics.lock().unwrap();

    m.faults_received += 1;
    s.fault_log.push(fault_msg);

    // Lab 7: match on enum variant (mirrors the fault_injector match in OCS)
    match kind
    {
        OcsFaultKind::MissionAbort =>
        {
            let alert = format!(
                "[{}ms] !!! CRITICAL GROUND ALERT !!! Mission Abort",
                now_ms()
            );
            println!("{alert}");
            m.critical_alerts.push(alert);

            if !s.fault_active
            {
                s.fault_active      = true;
                s.fault_detected_at = Some(Instant::now());
            }

            // Emergency halt bypasses typestate — this IS the fault-setter path
            let payload = encode_cmd(&GcsCmd
            {
                tag:      "cmd".into(),
                student:  STUDENT_ID.into(),
                cmd:      "EmergencyHalt".into(),
                ts:       now_ms(),
                priority: Some(1),
                iter:     None,
                generation:      None,
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
            if !s.fault_active
            {
                s.fault_active      = true;
                s.fault_detected_at = Some(Instant::now());
                println!("[FaultMgr] Safety interlock ENGAGED — commands blocked");
            }
        }

        OcsFaultKind::Unknown =>
        {
            println!("[FaultMgr] Unknown alert \"{event}\" — logged, interlock not engaged");
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
//  When misses exceed LOC_MISS_THRESHOLD → Loss of Contact.
// =============================================================================

async fn loss_of_contact_monitor_task(
    state:    Arc<Mutex<GcsState>>,
    metrics:  Arc<Mutex<GcsMetrics>>,
    cmd_tx:   mpsc::Sender<UplinkCmd>,
    shutdown: CancellationToken,
)
{
    println!("[LoC]      Monitor  threshold={LOC_MISS_THRESHOLD}");

    loop
    {
        // Lab 8: select between shutdown and the next check interval
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                println!("[LoC]      Exit.");
                return;
            }
            _ = sleep(Duration::from_millis(REREQUEST_INTERVAL_MS)) => {}
        }

        let ts = now_ms();

        // Read current miss counts and timestamps
        let (th, vel, att, last_th, last_vel, last_att, loc);
        {
            let s   = state.lock().unwrap();
            th      = s.thermal_misses;
            vel     = s.velocity_misses;
            att     = s.attitude_misses;
            last_th = s.last_thermal_ms;
            last_vel= s.last_velocity_ms;
            last_att= s.last_attitude_ms;
            loc     = s.loss_of_contact;
        }

        // Increment miss counters for any silent telemetry channel
        {
            let mut s = state.lock().unwrap();
            if ts.saturating_sub(last_th)  > TELEM_WATCHDOG_MS { s.thermal_misses  += 1; }
            if ts.saturating_sub(last_vel) > TELEM_WATCHDOG_MS { s.velocity_misses += 1; }
            if ts.saturating_sub(last_att) > TELEM_WATCHDOG_MS { s.attitude_misses += 1; }
        }

        let max_m = th.max(vel).max(att);

        if max_m >= LOC_MISS_THRESHOLD && !loc
        {
            let alert = format!(
                "[{ts}ms] !!! LOSS OF CONTACT !!!  th={th} vel={vel} att={att}"
            );
            println!("{alert}");
            metrics.lock().unwrap().critical_alerts.push(alert);
            state.lock().unwrap().loss_of_contact = true;
            metrics.lock().unwrap().missed_packets += max_m as u64;

            // serde: encode a re-request command and send to OCS
            let payload = encode_cmd(&GcsCmd
            {
                tag:      "cmd".into(),
                student:  STUDENT_ID.into(),
                cmd:      "RetransmitRequest".into(),
                ts,
                priority: Some(1),
                iter:     None,
                generation:      None,
            });
            let _ = cmd_tx.try_send(UplinkCmd
            {
                payload,
                priority:   1,
                created_at: Instant::now(),
            });

            println!("[LoC]      Re-request sent.");
        }
        else if loc && max_m < LOC_MISS_THRESHOLD
        {
            state.lock().unwrap().loss_of_contact = false;
            println!("[LoC]      Contact restored.");
        }
    }
}


// =============================================================================
//  FAULT MANAGER  (Lab 3: interlock latency timing)
//
//  Monitors the fault_active flag and measures how long it takes from
//  fault detection to interlock engagement (Lab 3 latency measurement).
//  If the interlock takes longer than FAULT_RESPONSE_LIMIT_MS → critical alert.
// =============================================================================

async fn fault_manager_task(
    state:    Arc<Mutex<GcsState>>,
    metrics:  Arc<Mutex<GcsMetrics>>,
    shutdown: CancellationToken,
)
{
    println!("[FaultMgr] Manager  response_limit={}ms", FAULT_RESPONSE_LIMIT_MS);

    loop
    {
        // Lab 8: select between shutdown and polling interval
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                println!("[FaultMgr] Exit.");
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

            // Lock metrics briefly, then release
            metrics.lock().unwrap().interlock_latency_ms.push(interlock_ms);  // (lab 2)

            if interlock_ms > FAULT_RESPONSE_LIMIT_MS
            {
                let alert = format!(
                    "[{}ms] !!! CRITICAL ALERT !!! Interlock {interlock_ms}ms > {FAULT_RESPONSE_LIMIT_MS}ms",
                    now_ms()
                );
                println!("{alert}");

                // Lock metrics briefly, then release
                metrics.lock().unwrap().critical_alerts.push(alert);  // (lab 2)

                // Lock state to write — safe now because NO other lock is held
                {
                    let mut s = state.lock().unwrap();  // (lab 2 — Arc<Mutex<T>> write)
                    s.fault_active      = false;
                    s.fault_detected_at = None;
                }  // lock released here
                println!("[FaultMgr] Interlock auto-cleared.");
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
//    1. tokio::select! { shutdown | sleep(period) }           (Lab 8)
//    2. Check fault_active — reject if faulted                (Lab 7)
//    3. Construct GcsMode::<Normal>::new() only when OK        (Lab 7)
//    4. Call dispatch_command(&mode, ...) — compile enforced   (Lab 7)
//    5. Encode command with serde_json::to_string(&GcsCmd{..}) (serde)
//    6. Measure and record jitter                             (Lab 2)
// =============================================================================

// ── Thermal Check (RM P1) — fires every 50ms ─────────────────────────────────
async fn thermal_command_task(
    cmd_tx:   mpsc::Sender<UplinkCmd>,
    state:    Arc<Mutex<GcsState>>,
    metrics:  Arc<Mutex<GcsMetrics>>,
    shutdown: CancellationToken,
)
{
    // This is the heartbeat() pattern from Lab 6 — a periodic async task
    println!("[ThermalCmd] period={}ms  RM-P1", THERMAL_CMD_PERIOD_MS);

    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        let t0 = Instant::now();

        // Lab 8: wait for either shutdown OR the period timer
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                println!("[ThermalCmd] Exit.");
                return;
            }
            _ = sleep(Duration::from_millis(THERMAL_CMD_PERIOD_MS)) => {}
        }

        // ── Jitter Calculation (Lab 2) ────────────────────────────────────────
        let expected_ms = iter * THERMAL_CMD_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // ── Lab 7: Typestate gate — only dispatch if not faulted ─────────────
        if state.lock().unwrap().fault_active
        {
            let rej = format!(
                "[{}ms] [REJECT] ThermalCheck iter={iter} — FaultLocked",
                now_ms()
            );
            println!("{rej}");
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
        let payload = encode_cmd(&GcsCmd
        {
            tag:      "cmd".into(),
            student:  STUDENT_ID.into(),
            cmd:      "ThermalCheck".into(),
            ts:       now_ms(),
            priority: Some(1),
            iter:     Some(iter),
            generation:      None,
        });

        // Lab 7: dispatch_command takes &GcsMode<Normal> — compile-time safety
        dispatch_command(&mode, &cmd_tx, payload, 1);

        let jitter_us = (drift_ms * 1_000).unsigned_abs() as i64;

        if jitter_us > UPLINK_JITTER_LIMIT_US
        {
            let v = format!(
                "[{}ms] [WARN] ThermalCmd jitter {jitter_us}µs iter={iter}",
                now_ms()
            );
            println!("{v}");
            metrics.lock().unwrap().deadline_violations.push(v);
        }

        {
            let mut m = metrics.lock().unwrap();
            m.thermal_jitter_us.push(jitter_us);
            m.drift_ms.push(drift_ms);
            m.active_ms  += t0.elapsed().as_millis() as u64;
            m.elapsed_ms  = task_start.elapsed().as_millis() as u64;
        }

        if iter % 20 == 0
        {
            println!(
                "[ThermalCmd] iter={iter:>4}  drift={drift_ms:+}ms  jitter={jitter_us}µs"
            );
        }

        iter += 1;
    }
}

// ── Velocity Check (RM P2) — fires every 120ms ────────────────────────────────
async fn velocity_command_task(
    cmd_tx:   mpsc::Sender<UplinkCmd>,
    state:    Arc<Mutex<GcsState>>,
    metrics:  Arc<Mutex<GcsMetrics>>,
    shutdown: CancellationToken,
)
{
    println!("[VelocityCmd] period={}ms  RM-P2", VELOCITY_CMD_PERIOD_MS);

    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        let t0 = Instant::now();

        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                println!("[VelocityCmd] Exit.");
                return;
            }
            _ = sleep(Duration::from_millis(VELOCITY_CMD_PERIOD_MS)) => {}
        }

        let expected_ms = iter * VELOCITY_CMD_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Lab 7: reject if faulted
        if state.lock().unwrap().fault_active
        {
            let rej = format!(
                "[{}ms] [REJECT] VelocityCheck iter={iter} — FaultLocked",
                now_ms()
            );
            println!("{rej}");
            let mut m = metrics.lock().unwrap();
            m.commands_rejected += 1;
            m.rejection_log.push(rej);
            iter += 1;
            continue;
        }

        let mode    = GcsMode::<Normal>::new();
        let payload = encode_cmd(&GcsCmd
        {
            tag:      "cmd".into(),
            student:  STUDENT_ID.into(),
            cmd:      "VelocityCheck".into(),
            ts:       now_ms(),
            priority: Some(2),
            iter:     Some(iter),
            generation:      None,
        });

        dispatch_command(&mode, &cmd_tx, payload, 2);

        let jitter_us = (drift_ms * 1_000).unsigned_abs() as i64;
        {
            let mut m = metrics.lock().unwrap();
            m.velocity_jitter_us.push(jitter_us);
            m.drift_ms.push(drift_ms);
            m.active_ms += t0.elapsed().as_millis() as u64;
        }

        if iter % 8 == 0
        {
            println!(
                "[VelocityCmd] iter={iter:>4}  drift={drift_ms:+}ms  jitter={jitter_us}µs"
            );
        }

        iter += 1;
    }
}


// =============================================================================
//  LAB 8 PART 1 — FRAGILE ATTITUDE DISPATCHER
//
//  This is the fragile_worker pattern from Lab 8 applied to an async task.
//  It has a 2% random chance of panicking each iteration, simulating a
//  transmitter hardware glitch.
//
//  Like fragile_gyroscope in OCS, it runs in an infinite loop — the
//  supervisor detects the panic and restarts it.
// =============================================================================

async fn fragile_attitude_dispatcher(
    generation: u32,
    cmd_tx:     mpsc::Sender<UplinkCmd>,
    state:      Arc<Mutex<GcsState>>,
    metrics:    Arc<Mutex<GcsMetrics>>,
)
{
    println!(
        "[AttCmd-{generation}]  period={}ms  RM-P3  fragile",
        ATTITUDE_CMD_PERIOD_MS
    );

    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        // Lab 6: async sleep — yields execution so other tasks can run
        sleep(Duration::from_millis(ATTITUDE_CMD_PERIOD_MS)).await;

        // Lab 8: 2% random panic (fragile_worker pattern from Lab 8)
        if rand::random_range(0u32..100) < 2
        {
            println!("[AttCmd-{generation}]  Transmitter glitch — panicking!");
            panic!("Attitude dispatcher fault (gen {generation})");
            // attitude_supervisor_task catches this via handle.await returning Err
        }

        let expected_ms = iter * ATTITUDE_CMD_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Lab 7: reject if faulted
        if state.lock().unwrap().fault_active
        {
            let rej = format!(
                "[{}ms] [REJECT] AttitudeCheck gen={generation} iter={iter}",
                now_ms()
            );
            println!("{rej}");
            let mut m = metrics.lock().unwrap();
            m.commands_rejected += 1;
            m.rejection_log.push(rej);
            iter += 1;
            continue;
        }

        let mode    = GcsMode::<Normal>::new();
        let payload = encode_cmd(&GcsCmd
        {
            tag:       "cmd".into(),
            student:   STUDENT_ID.into(),
            cmd:       "AttitudeCheck".into(),
            ts:        now_ms(),
            priority:  Some(3),
            iter:      Some(iter),
            generation:Some(generation),
        });

        dispatch_command(&mode, &cmd_tx, payload, 3);

        let jitter_us = (drift_ms * 1_000).unsigned_abs() as i64;
        {
            let mut m = metrics.lock().unwrap();
            m.attitude_jitter_us.push(jitter_us);
            m.drift_ms.push(drift_ms);
        }

        if iter % 3 == 0
        {
            println!(
                "[AttCmd-{generation}]  iter={iter:>4}  drift={drift_ms:+}ms  jitter={jitter_us}µs"
            );
        }

        iter += 1;
    }
}


// =============================================================================
//  LAB 8 PART 2 — ATTITUDE SUPERVISOR  (async version of Lab 8 supervisor)
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

async fn attitude_supervisor_task(
    cmd_tx:   mpsc::Sender<UplinkCmd>,
    state:    Arc<Mutex<GcsState>>,
    metrics:  Arc<Mutex<GcsMetrics>>,
    shutdown: CancellationToken,
)
{
    println!("[AttSup]   Supervisor online.");

    let mut generation: u32 = 0;

    loop
    {
        // Check if we should exit before spawning a new generation
        if shutdown.is_cancelled()
        {
            println!("[AttSup]   Exit.");
            return;
        }

        generation += 1;
        println!("[AttSup]   Starting generation {generation}...");

        // Lab 8: spawn the fragile dispatcher as a new async task
        let handle = tokio::spawn(fragile_attitude_dispatcher(
            generation,
            cmd_tx.clone(),
            Arc::clone(&state),
            Arc::clone(&metrics),
        ));

        // Lab 8: match handle.await to detect panic vs normal exit
        match handle.await
        {
            Ok(_) =>
            {
                // Normal exit — shouldn't happen in loop{}, means it was cancelled
                println!("[AttSup]   Normal exit — done.");
                return;
            }
            Err(e) if e.is_panic() =>
            {
                // Panic detected — same restart logic as Lab 8
                metrics.lock().unwrap().attitude_restarts += 1;
                println!("[AttSup]   Panic! Restarting in 1s...");

                // Lab 8: backoff before restarting
                sleep(Duration::from_secs(1)).await;
            }
            Err(_) =>
            {
                // Cancelled — clean exit
                println!("[AttSup]   Cancelled.");
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
    metrics:  Arc<Mutex<GcsMetrics>>,
    state:    Arc<Mutex<GcsState>>,
    shutdown: CancellationToken,
)
{
    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() => return,
            _ = sleep(Duration::from_secs(10)) => {}
        }

        print_report(&metrics.lock().unwrap(), &state.lock().unwrap());
    }
}

fn print_report(m: &GcsMetrics, s: &GcsState)
{
    let reject_pct = if m.commands_sent + m.commands_rejected > 0
    {
        m.commands_rejected as f64
            / (m.commands_sent + m.commands_rejected) as f64
            * 100.0
    }
    else
    {
        0.0
    };

    let cpu = if m.elapsed_ms > 0
    {
        m.active_ms as f64 / m.elapsed_ms as f64 * 100.0
    }
    else
    {
        0.0
    };

    println!("\n╔══════════════════════════════════════════════════════╗");
    println!( "║  GCS REPORT  at {}ms", now_ms());
    println!( "╠══════════════════════════════════════════════════════╣");
    println!(
        "║  Mode: {}  LoC: {}  Att restarts: {}",
        if s.fault_active { "FaultLocked" } else { "Normal" },
        s.loss_of_contact,
        m.attitude_restarts
    );
    println!(
        "║  Telemetry: rx={}  missed={}",
        m.telemetry_received, m.missed_packets
    );
    println!(
        "║  Decode latency (µs)  deadline={}ms  (spawn_blocking)",
        DECODE_DEADLINE_MS
    );
    let dl: Vec<i64> = m.decode_latency_us.iter().map(|&v| v as i64).collect();
    print_stat_row("Decode (µs)", &dl);
    println!(
        "║  Commands  sent={}  rejected={} ({:.1}%)",
        m.commands_sent, m.commands_rejected, reject_pct
    );
    println!(
        "║  Dispatch latency (µs)  deadline={}ms",
        DISPATCH_DEADLINE_MS
    );
    let disp: Vec<i64> = m.dispatch_latency_us.iter().map(|&v| v as i64).collect();
    print_stat_row("Dispatch (µs)", &disp);
    println!("║  Uplink jitter (µs)  limit={}µs", UPLINK_JITTER_LIMIT_US);
    print_stat_row("ThermalCheck  RM-P1",  &m.thermal_jitter_us);
    print_stat_row("VelocityCheck RM-P2",  &m.velocity_jitter_us);
    print_stat_row("AttitudeCheck RM-P3",  &m.attitude_jitter_us);
    println!("║  Drift (ms)");
    print_stat_row("All tasks", &m.drift_ms);
    println!(
        "║  Faults: {}  Critical alerts: {}",
        m.faults_received, m.critical_alerts.len()
    );
    if !m.interlock_latency_ms.is_empty()
    {
        let il: Vec<i64> = m.interlock_latency_ms.iter().map(|&v| v as i64).collect();
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

    // ── Shared State Setup ────────────────────────────────────────────────────
    // Lab 2: Arc<Mutex<T>> — shared ownership with mutual exclusion
    let state    = Arc::new(Mutex::new(GcsState::default()));
    let metrics  = Arc::new(Mutex::new(GcsMetrics::default()));

    // Lab 8: CancellationToken — clean shutdown signal sent to all tasks
    let shutdown = CancellationToken::new();

    // ── Lab 9: Async UDP Sockets ──────────────────────────────────────────────
    // tokio::net::UdpSocket is the async version of the std::net::UdpSocket
    // used in Lab 9's blocking server code.
    let recv_sock = Arc::new(
        UdpSocket::bind(GCS_TELEM_BIND)
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
    println!("      Receive          : {GCS_TELEM_BIND}  (serde_json decoded)");
    println!("      Send to          : {OCS_CMD_ADDR}  (serde_json encoded)");
    println!("      Typestate        : GcsMode<Normal/FaultLocked>  (Lab 7 PhantomData)");
    println!("      Decode           : spawn_blocking  (Lab 6 CPU-off-async)");
    println!(
        "      RM periods       : thermal={}ms  vel={}ms  att={}ms",
        THERMAL_CMD_PERIOD_MS, VELOCITY_CMD_PERIOD_MS, ATTITUDE_CMD_PERIOD_MS
    );
    println!("      Sim duration     : {SIM_DURATION_S}s\n");

    // ── Lab 6: tokio::spawn for each subsystem ────────────────────────────────
    // Each spawn creates a lightweight async task (NOT an OS thread).
    // Tasks cooperatively yield when they call .await, allowing others to run.

    // UDP receiver — listens for OCS telemetry, feeds the incoming channel
    tokio::spawn(udp_receiver_loop(
        Arc::clone(&recv_sock), incoming_tx, shutdown.clone()
    ));

    // UDP sender — consumes command queue, fires packets to OCS
    tokio::spawn(udp_sender_task(
        cmd_rx, Arc::clone(&send_sock), Arc::clone(&metrics), shutdown.clone()
    ));

    // Telemetry processor — decodes packets (spawn_blocking) and routes them
    tokio::spawn(telemetry_processor_task(
        incoming_rx, Arc::clone(&state), Arc::clone(&metrics),
        cmd_tx.clone(), shutdown.clone(),
    ));

    // Loss-of-contact monitor — watchdog for telemetry silence
    tokio::spawn(loss_of_contact_monitor_task(
        Arc::clone(&state), Arc::clone(&metrics),
        cmd_tx.clone(), shutdown.clone(),
    ));

    // Fault manager — measures interlock response latency
    tokio::spawn(fault_manager_task(
        Arc::clone(&state), Arc::clone(&metrics), shutdown.clone(),
    ));

    // RM command tasks (Lab 7 typestate + Lab 8 supervisor)
    tokio::spawn(thermal_command_task(
        cmd_tx.clone(), Arc::clone(&state), Arc::clone(&metrics), shutdown.clone(),
    ));

    tokio::spawn(velocity_command_task(
        cmd_tx.clone(), Arc::clone(&state), Arc::clone(&metrics), shutdown.clone(),
    ));

    // Attitude supervisor wraps the fragile dispatcher (Lab 8)
    tokio::spawn(attitude_supervisor_task(
        cmd_tx.clone(), Arc::clone(&state), Arc::clone(&metrics), shutdown.clone(),
    ));

    // Periodic metrics reporter
    tokio::spawn(metrics_reporter_task(
        Arc::clone(&metrics), Arc::clone(&state), shutdown.clone(),
    ));

    println!("[GCS] All tasks online.  Waiting for OCS telemetry...\n");

    // ── Run for the simulation duration ──────────────────────────────────────
    sleep(Duration::from_secs(SIM_DURATION_S)).await;

    // ── Lab 8: Graceful Shutdown via CancellationToken ────────────────────────
    // Calling shutdown.cancel() wakes every tokio::select! branch that is
    // waiting on shutdown.cancelled() — all tasks exit cleanly.
    println!("\n[GCS] Simulation ended — shutting down...");
    shutdown.cancel();

    // Give tasks a moment to finish cleanup before printing the final report
    sleep(Duration::from_millis(500)).await;

    // Print final summary
    println!("\n[GCS] FINAL REPORT:");
    print_report(&metrics.lock().unwrap(), &state.lock().unwrap());
    println!("[GCS] Done.");
}