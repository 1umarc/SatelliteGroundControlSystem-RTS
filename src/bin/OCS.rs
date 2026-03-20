// =============================================================================
//  src/bin/OCS.rs   —   Satellite Onboard Control System (OCS)
//  CT087-3-3 Real-Time Systems  |  Student A
//
//  Run:   cargo run --bin OCS --release   (start before GCS)
//
//  ARCHITECTURE OVERVIEW:
//  This is a HARD Real-Time System built on OS threads (std::thread).
//  As covered in Lab 6, tokio is "Soft Real-Time". OS threads are truly
//  preemptible by the kernel scheduler, which is what Hard RTS requires.
//
//  IPC (Inter-Process Communication):
//    OCS  -->  GCS telemetry : 127.0.0.1:9000
//    GCS  -->  OCS commands  : 0.0.0.0:9001
//
//  SERDE / SERDE_JSON:
//  All UDP payloads are typed Rust structs, serialised with serde_json.
//  This replaces fragile format!("{{\"tag\":\"thermal\",...}}") strings.
//  The wire format is identical — serde just enforces it at compile time.
//
//  LAB REFERENCE MAP:
//  ─────────────────────────────────────────────────────────────
//  Lab 1  const, structs, enums, match, Vec, mut
//  Lab 2  std::thread::spawn + join, Arc<Mutex<T>>, Arc::clone(),
//         jitter = |actual_elapsed − expected_elapsed|
//  Lab 3  Vec::with_capacity(MAX) — no runtime realloc
//         Instant::now() + .elapsed() for latency measurement
//  Lab 7  PhantomData<S> typestate: Radio<Idle> → Radio<Transmitting>
//         enum FaultType + match, consecutive-miss counter reset
//  Lab 8  Supervisor: thread::spawn + match handle.join()
//         Fragile gyroscope task that panics randomly
//  Lab 9  std::net::UdpSocket — send_to / recv_from (blocking)
//  Lab 11 ScheduledThreadPool for RM background tasks
//         std::sync::mpsc — tx.clone() / send / for msg in rx
//         Arc<Mutex<Vec>> downlink queue + .pop() drain
// =============================================================================

// ── Standard Library Imports ─────────────────────────────────────────────────
use std::collections::BinaryHeap;   // for priority queue (Lab 11)
use std::marker::PhantomData;       // for typestate pattern (Lab 7)
use std::net::UdpSocket;            // for blocking UDP socket (Lab 9)
use std::sync::{Arc, Mutex};        // for shared ownership across threads (Lab 2)
use std::sync::mpsc;                // for message passing between threads (Lab 11)
use std::thread;                    // for OS thread creation (Lab 2)
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ── External Crate Imports ────────────────────────────────────────────────────
use rand;                               // random number generation (Lab 8 fault simulation)
use scheduled_thread_pool::ScheduledThreadPool;  // thread pool for RM tasks (Lab 11)
use serde::{Deserialize, Serialize};    // automatic JSON serialisation/deserialisation
use serde_json;                         // JSON encode/decode for UDP payloads


// =============================================================================
//  PART 0 — CONSTANTS   (Lab 1: const keyword)
//
//  Constants are declared with `const` instead of `let`.
//  They must have an explicit type and cannot be mutable.
//  They live for the entire program lifetime (no ownership).
// =============================================================================

// ── Sensor Sampling Periods (milliseconds) ────────────────────────────────────
const GYRO_PERIOD_MS:    u64 = 20;    // highest priority sensor, fastest rate
const ACCEL_PERIOD_MS:   u64 = 50; // FIXME: Accelerometer 
const THERMAL_PERIOD_MS: u64 = 100;

// ── Background Task Periods (milliseconds) ────────────────────────────────────
const HEALTH_PERIOD_MS:   u64 = 200;
const COMPRESS_PERIOD_MS: u64 = 500;
const ANTENNA_PERIOD_MS:  u64 = 1_000; //TODO: 1000, can chg

// ── Timing and Safety Thresholds ─────────────────────────────────────────────
const JITTER_LIMIT_US:    i64   = 1_000;  // warn if jitter exceeds 1ms (Lab 2) - US = microseconds
const MAX_CONSEC_MISSES:  u32   = 3;      // safety alert after 3 consecutive drops
const BUFFER_CAPACITY:    usize = 100;    // max items in the priority buffer
const DEGRADED_THRESHOLD: f32   = 0.80;  // enter degraded mode at 80% full

// ── Downlink / Radio Parameters ───────────────────────────────────────────────
const VISIBILITY_INTERVAL_S:  u64 = 10; // 10 seconds //XXX: new
const DOWNLINK_WINDOW_MS:     u64 = 30;
const DOWNLINK_INIT_LIMIT_MS: u64 = 5;

// ── Fault Injection and Simulation ───────────────────────────────────────────
const FAULT_INTERVAL_S:  u64 = 60;
const RECOVERY_LIMIT_MS: u64 = 200;
const SIM_DURATION_S:    u64 = 180; // XXX: 180 seconds = 3 minutes

// ── Lab 3: Pre-allocated capacities (no runtime realloc during simulation) ────
// We call Vec::with_capacity() at startup so the Vec never needs to grow
// during the simulation, avoiding unpredictable heap allocation latency.
const MAX_JITTER_SAMPLES:  usize = 10_000;
const MAX_DRIFT_SAMPLES:   usize = 20_000;
const MAX_LATENCY_SAMPLES: usize = 20_000;
const MAX_LOG_ENTRIES:     usize = 500;

// ── Network Addresses ─────────────────────────────────────────────────────────
const GCS_TELEM_ADDR: &str = "127.0.0.1:9000";
const OCS_CMD_BIND:   &str = "0.0.0.0:9001";
const UDP_TIMEOUT_MS: u64  = 100;
const STUDENT_ID:     &str = "tp071542";


// =============================================================================
//  SERDE MESSAGE TYPES
//
//  Every UDP payload the OCS sends is one of these enum variants.
//  #[serde(tag = "tag")] writes {"tag":"thermal",...} on the wire —
//  the same format as the old manual format! strings, but now the compiler
//  checks every field name and type at compile time.
//
//  The GCS deserialises with serde_json::from_str::<OcsMsg>(&payload),
//  matching on the same enum — no more payload.contains("\"tag\":\"alert\"").
// =============================================================================

// The main message enum — each variant maps to a different telemetry type
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "tag", rename_all = "snake_case")]
enum OcsMsg
{
    Thermal  { student: String, seq: u64, temp: f64,    drift_ms: i64 },
    Accel    { student: String, seq: u64, mag:  f64                    },
    Gyro     { student: String, generation: u32, seq:  u64, omega_z:    f64   },
    Status   { student: String, iter: u64, fill: f64, state: String, drift_ms: i64 },
    Downlink { student: String, pkt: u64, bytes: usize, q_lat_ms: u64  },

    // Alert uses #[serde(flatten)] so AlertInfo fields appear directly
    // in the JSON object alongside "student" and "event", rather than nested.
    Alert    { student: String, event: String, #[serde(flatten)] info: AlertInfo },
}

// Optional extra fields for Alert messages.
// We use Option<T> so absent fields are omitted (skip_serializing_if) rather
// than written as null — keeps the wire format clean.
#[derive(Serialize, Deserialize, Debug, Default)]
struct AlertInfo
{
    #[serde(skip_serializing_if = "Option::is_none")]
    pub misses:     Option<u32>,     // for thermal miss alerts

    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,     // for gyro restart alerts

    #[serde(skip_serializing_if = "Option::is_none")]
    pub count:      Option<u32>,     // fault occurrence count

    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault_type: Option<String>,  // e.g. "SensorBusHang", "PowerSpike"
}

// Command messages arriving FROM the GCS. The OCS only needs tag/cmd/ts.
#[derive(Serialize, Deserialize, Debug)]
struct GcsCmd
{
    pub tag:     String,   // always "cmd"
    pub student: String,
    pub cmd:     String,   // e.g. "ThermalCheck", "EmergencyHalt"
    pub ts:      u64,      // Unix timestamp in ms
}

// Helper: serialise an OcsMsg to a JSON string.
// If encoding fails (shouldn't happen), logs the error and returns "".
fn encode(msg: &OcsMsg) -> String
{
    serde_json::to_string(msg)
        .unwrap_or_else(|e|
        {
            println!("[ENCODE] {e}");
            String::new()
        })
}


// =============================================================================
//  DATA TYPES   (Lab 1: structs, enums)
// =============================================================================

// Which physical sensor produced a reading
#[derive(Debug, Clone, PartialEq)]
enum SensorType
{
    Thermal,
    Accelerometer,
    Gyroscope,
}

// One sensor measurement, timestamped and prioritised
// Lab 1: struct with multiple fields and explicit types
#[derive(Debug, Clone)]
struct SensorReading
{
    sensor_type:  SensorType,
    value:        f64,
    timestamp_ms: u64,
    priority:     u8,    // lower number = higher priority (1 is most critical)
    sequence_num: u64,
}

// ── BinaryHeap ordering for SensorReading ────────────────────────────────────
// Rust's BinaryHeap is a MAX-heap by default.
// We flip the comparison so that LOWER priority numbers come out first
// (i.e. priority 1 = thermal = most urgent pops before priority 3 = gyro).
// Lab 1: impl blocks for custom traits
impl PartialEq  for SensorReading
{
    fn eq(&self, o: &Self) -> bool { self.priority == o.priority }
}
impl Eq for SensorReading {}

impl PartialOrd for SensorReading
{
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering>
    {
        Some(self.cmp(o))
    }
}

impl Ord for SensorReading
{
    // Reverse the comparison: lower priority number → higher heap position
    fn cmp(&self, o: &Self) -> std::cmp::Ordering
    {
        o.priority.cmp(&self.priority)
    }
}

// A compressed packet ready for downlink transmission
// Derives Serialize/Deserialize for optional external inspection
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DataPacket
{
    packet_id:     u64,
    payload:       String,  // serde_json batch of sensor readings
    created_at_ms: u64,     // used to measure queue latency
    size_bytes:    usize,
}

// Overall system health state (Lab 1: enum)
#[derive(Debug, Clone, PartialEq)]
enum SystemState
{
    Normal,
    Degraded,
    MissionAbort,
}

// Types of faults the injector can simulate (Lab 7: enum FaultType,
// mirrors SensorError { Glitch, PowerFailure } from Lab 7)
#[derive(Debug)]
enum FaultType
{
    SensorBusHang(u64),   // payload = hang duration in ms
    CorruptedReading,
    PowerSpike,
}


// =============================================================================
//  LAB 7 — RADIO TYPESTATE  (PhantomData<S>)
//
//  This is the same pattern as the Door typestate in Lab 7.
//
//  Door analogy → Radio analogy:
//    Door<Locked>       →  Radio<Idle>
//    Door<Unlocked>     →  Radio<Transmitting>
//    door.unlock()      →  radio.initialise()
//    door.lock()        →  radio returns to Idle after transmit()
//
//  The key point: calling .transmit() on Radio<Idle> is a COMPILE ERROR.
//  The state machine is enforced at compile time, not runtime.
//
//  PhantomData<S> tells the compiler "this struct is parameterised by S"
//  without storing any actual data for S at runtime (zero cost).
// =============================================================================

// The two possible radio states
struct Idle;
struct Transmitting;

// The generic Radio struct — S is the current state
struct Radio<S>
{
    _state: PhantomData<S>   // zero-size marker; only used by the type system
}

// Methods available ONLY when the radio is Idle (Lab 7: impl Door<Locked>)
impl Radio<Idle>
{
    // Create a new radio, always starts Idle
    fn new() -> Self
    {
        Radio { _state: PhantomData }
    }

    // Idle → Transmitting.
    // Returns None if hardware init exceeds the 5ms deadline (hard deadline).
    // Lab 7 analogy: door.unlock() → Door<Unlocked>
    fn initialise(self) -> Option<Radio<Transmitting>>
    {
        let t0 = Instant::now();
        thread::sleep(Duration::from_millis(3));  // simulate hardware init delay

        let init_ms = t0.elapsed().as_millis() as u64;

        if init_ms > DOWNLINK_INIT_LIMIT_MS
        {
            println!("[Radio]    Init {init_ms}ms > {DOWNLINK_INIT_LIMIT_MS}ms — staying Idle");
            None
        }
        else
        {
            println!("[Radio]    Init OK ({init_ms}ms) — Transmitting");
            Some(Radio { _state: PhantomData })
        }
    }
}

// Methods available ONLY when the radio is Transmitting (Lab 7: impl Door<Open>)
impl Radio<Transmitting>
{
    // Transmit all queued packets, then return to Idle state.
    // Lab 7 analogy: door.close() → Door<Unlocked>
    // Note: self is consumed (moved), enforcing the state transition.
    fn transmit(
        self,
        packets: &[DataPacket],
        metrics: &Arc<Mutex<SystemMetrics>>,
        udp_tx:  &mpsc::Sender<String>,
    ) -> Radio<Idle>
    {
        let tx_t        = Instant::now();
        let mut tx_done = 0usize;
        let mut total_b = 0usize;

        for pkt in packets
        {
            // Hard deadline: we must finish within the downlink window
            if tx_t.elapsed().as_millis() as u64 >= DOWNLINK_WINDOW_MS
            {
                let v = format!(
                    "[{}ms] [DEADLINE] TX window exceeded after {tx_done} pkts",
                    now_ms()
                );
                println!("{v}");
                metrics.lock().unwrap().deadline_violations.push(v);
                break;
            }

            // Queue latency = time since this packet was created
            let q_lat = now_ms().saturating_sub(pkt.created_at_ms);
            total_b  += pkt.size_bytes;
            tx_done  += 1;

            println!(
                "[Downlink] TX pkt={}  {}B  q_lat={}ms",
                pkt.packet_id, pkt.size_bytes, q_lat
            );

            // serde: send downlink metadata to GCS via UDP
            let msg = encode(&OcsMsg::Downlink
            {
                student:  STUDENT_ID.into(),
                pkt:      pkt.packet_id,
                bytes:    pkt.size_bytes,
                q_lat_ms: q_lat,
            });
            let _ = udp_tx.send(msg);
        }

        let tx_ms = tx_t.elapsed().as_millis() as u64;
        println!(
            "[Downlink] Done  {tx_done}/{} pkts  {total_b}B  {tx_ms}ms",
            packets.len()
        );

        // Return the Idle state — caller now holds Radio<Idle>
        Radio { _state: PhantomData }
    }
}


// =============================================================================
//  SHARED METRICS  (Lab 3: Vec::with_capacity, no runtime realloc)
//
//  All metric Vecs are pre-allocated at startup with a known worst-case
//  capacity. This means push() will NEVER trigger a heap reallocation
//  during the simulation, removing a source of unpredictable latency.
//  This is the Lab 3 lesson: prefer stack or pre-allocated heap.
// =============================================================================

struct SystemMetrics
{
    // Jitter samples per sensor (Lab 2: jitter = |actual - expected|)
    thermal_jitter_us:   Vec<i64>,
    accel_jitter_us:     Vec<i64>,
    gyro_jitter_us:      Vec<i64>,

    // Task scheduling drift and buffer insert latency (Lab 3)
    drift_ms:            Vec<i64>,
    insert_latency_us:   Vec<u64>,

    // Recovery and fault tracking
    recovery_times_ms:   Vec<u64>,
    dropped_log:         Vec<String>,
    deadline_violations: Vec<String>,
    fault_log:           Vec<String>,
    safety_alerts:       Vec<String>,

    // Counters
    total_received:        u64,
    total_dropped:         u64,
    consec_thermal_misses: u32,   // Lab 7: consecutive miss counter, reset on success
    active_ms:             u64,
    elapsed_ms:            u64,
    gyro_restarts:         u32,   // Lab 8: how many times supervisor restarted gyro
}

impl SystemMetrics
{
    // Lab 3: allocate all Vecs ONCE at startup with worst-case capacity.
    // During the simulation, no Vec will need to grow (no realloc).
    fn new() -> Self
    {
        SystemMetrics
        {
            thermal_jitter_us:   Vec::with_capacity(MAX_JITTER_SAMPLES),
            accel_jitter_us:     Vec::with_capacity(MAX_JITTER_SAMPLES),
            gyro_jitter_us:      Vec::with_capacity(MAX_JITTER_SAMPLES),
            drift_ms:            Vec::with_capacity(MAX_DRIFT_SAMPLES),
            insert_latency_us:   Vec::with_capacity(MAX_LATENCY_SAMPLES),
            recovery_times_ms:   Vec::with_capacity(10),
            dropped_log:         Vec::with_capacity(MAX_LOG_ENTRIES),
            deadline_violations: Vec::with_capacity(MAX_LOG_ENTRIES),
            fault_log:           Vec::with_capacity(MAX_LOG_ENTRIES),
            safety_alerts:       Vec::with_capacity(MAX_LOG_ENTRIES),
            total_received:        0,
            total_dropped:         0,
            consec_thermal_misses: 0,
            active_ms:             0,
            elapsed_ms:            0,
            gyro_restarts:         0,
        }
    }
}


// =============================================================================
//  BOUNDED PRIORITY BUFFER  (Lab 11 concept upgrade)
//
//  In Lab 11, jobs were stored in a plain Arc<Mutex<Vec<Job>>>.
//  Here we upgrade that to a BinaryHeap so that the highest-priority
//  sensor readings (thermal = priority 1) are always processed first,
//  regardless of insertion order.
//
//  The buffer also has a capacity limit — if full, push() returns false
//  and the caller must handle the drop (log it, trigger alert, etc.)
// =============================================================================

struct PriorityBuffer
{
    heap:     BinaryHeap<SensorReading>,  // max-heap with reversed ordering
    capacity: usize,
}

impl PriorityBuffer
{
    fn new(cap: usize) -> Self
    {
        PriorityBuffer
        {
            heap:     BinaryHeap::with_capacity(cap),
            capacity: cap,
        }
    }

    // Push a reading. Returns false (drop) if buffer is at capacity.
    fn push(&mut self, r: SensorReading) -> bool
    {
        if self.heap.len() >= self.capacity
        {
            return false;  // buffer full — caller handles the drop
        }
        self.heap.push(r);
        true
    }

    // Pop the highest-priority reading (lowest priority number wins)
    fn pop(&mut self) -> Option<SensorReading>
    {
        self.heap.pop()
    }

    // How full is the buffer? Used to transition to Degraded state.
    fn fill_ratio(&self) -> f32
    {
        self.heap.len() as f32 / self.capacity as f32
    }
}


// =============================================================================
//  HELPER FUNCTIONS
// =============================================================================

// Returns current Unix time in milliseconds — used for all timestamps
fn now_ms() -> u64
{
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// Pretty-print a single row in the metrics report table
// Lab 3: min, max, avg statistics on a slice of i64 samples
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


// =============================================================================
//  UDP SENDER THREAD  (Lab 9: UdpSocket.send_to + Lab 11: mpsc consumer)
//
//  This thread owns the UDP socket and consumes serialised strings from
//  a mpsc::Receiver channel. All other threads produce strings into the
//  channel (mpsc::Sender), and this thread fires them out over the network.
//
//  Lab 11 pattern: "for msg in rx" exits naturally when ALL senders have
//  been dropped (drop(tx) in main is the signal — same as Lab 11).
// =============================================================================

fn udp_sender_thread(rx: mpsc::Receiver<String>)
{
    // Bind to any available local port — we only need to SEND, not receive
    let sock = UdpSocket::bind("0.0.0.0:0").expect("[UDP-SEND] bind failed");
    println!("[UDP-SEND] Ready → {GCS_TELEM_ADDR}");

    // Lab 11: "for msg in rx" — the loop ends when all senders are dropped
    for msg in &rx
    {
        sock.send_to(msg.as_bytes(), GCS_TELEM_ADDR)
            .unwrap_or_else(|e|
            {
                println!("[UDP-SEND] {e}");
                0
            });
    }

    println!("[UDP-SEND] All senders dropped — exit.");
}


// =============================================================================
//  COMMAND RECEIVER THREAD  (Lab 9: UdpSocket.recv_from + serde_json)
//
//  Listens on OCS_CMD_BIND for JSON command messages from the GCS.
//  Uses a read timeout so the while-loop can check `running` periodically.
//  Lab 9: blocking UdpSocket.recv_from() with timeout set via set_read_timeout().
// =============================================================================

fn command_receiver_thread(running: Arc<Mutex<bool>>)
{
    let sock = UdpSocket::bind(OCS_CMD_BIND).expect("[OCS-CMD] bind failed");

    // set_read_timeout prevents recv_from from blocking forever,
    // allowing us to check the `running` flag each iteration.
    sock.set_read_timeout(Some(Duration::from_millis(UDP_TIMEOUT_MS))).unwrap();

    println!("[OCS-CMD] Listening on {OCS_CMD_BIND}");

    let mut buf = [0u8; 4096];

    while *running.lock().unwrap()
    {
        match sock.recv_from(&mut buf)
        {
            Ok((len, addr)) =>
            {
                let raw = String::from_utf8_lossy(&buf[..len]);

                // serde_json: deserialise the raw bytes into a typed GcsCmd struct
                match serde_json::from_str::<GcsCmd>(&raw)
                {
                    Ok(cmd)  => println!("[OCS-CMD] {addr} → cmd=\"{}\"  ts={}", cmd.cmd, cmd.ts),
                    Err(_)   => println!("[OCS-CMD] {addr} (unparsed): {raw}"),
                }
            }

            // WouldBlock / TimedOut are expected — just loop again
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => {}

            Err(e) => println!("[OCS-CMD] {e}"),
        }
    }

    println!("[OCS-CMD] Exit.");
}


// =============================================================================
//  PART 1 — SENSOR THREADS  (dedicated OS threads — Lab 2)
//
//  Each sensor runs in its own OS thread. Lab 2 showed us that OS threads
//  are truly preemptible — the kernel scheduler can pause and resume them
//  independently, which is essential for Hard RTS timing guarantees.
//
//  Jitter is measured the same way as Lab 2:
//    expected_wakeup = task_start + (seq * period)
//    actual_wakeup   = Instant::now()
//    jitter          = |actual_elapsed - expected_elapsed|
// =============================================================================

fn thermal_sensor_thread(
    buffer:    Arc<Mutex<PriorityBuffer>>,  // shared priority buffer (Lab 2: Arc<Mutex<T>>)
    metrics:   Arc<Mutex<SystemMetrics>>,   // shared metrics collector
    state:     Arc<Mutex<SystemState>>,     // shared system state
    emergency: Arc<Mutex<bool>>,            // emergency flag (set on too many misses)
    udp_tx:    mpsc::Sender<String>,        // channel to UDP sender thread (Lab 11)
    running:   Arc<Mutex<bool>>,            // shutdown signal
)
{
    println!("[Thermal]  period={}ms  buf_priority=1  SAFETY-CRITICAL", THERMAL_PERIOD_MS);

    let mut seq:    u64     = 0;
    let task_start: Instant = Instant::now();  // reference point for jitter calc (Lab 2)

    while *running.lock().unwrap()
    {
        thread::sleep(Duration::from_millis(THERMAL_PERIOD_MS));

        // ── Jitter Calculation (Lab 2) ────────────────────────────────────────
        // Compare when we ACTUALLY woke up vs when we SHOULD have woken up.
        let expected_ms = seq * THERMAL_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Simulate a slowly rising temperature
        let temp: f64 = 22.0 + (seq % 50) as f64 * 0.3;

        // Build the sensor reading with priority 1 (highest / most critical)
        let reading = SensorReading
        {
            sensor_type: SensorType::Thermal,
            value:        temp,
            timestamp_ms: now_ms(),
            priority:     1,   // thermal = safety-critical, must be processed first
            sequence_num: seq,
        };

        // ── Lab 3: Measure buffer insert latency ──────────────────────────────
        let t0       = Instant::now();
        let accepted = buffer.lock().unwrap().push(reading);
        let ins_us   = t0.elapsed().as_micros() as u64;

        // ── Update shared metrics ─────────────────────────────────────────────
        {
            let mut m = metrics.lock().unwrap();
            m.total_received    += 1;
            m.insert_latency_us.push(ins_us);
            m.drift_ms.push(drift_ms);

            // Record jitter (skip seq=0 as there is no previous reference)
            if seq > 0
            {
                let jitter_us = (drift_ms * 1_000).unsigned_abs() as i64;
                m.thermal_jitter_us.push(jitter_us);

                if jitter_us > JITTER_LIMIT_US
                {
                    let v = format!(
                        "[{}ms] [WARN] Thermal jitter {}µs  seq={seq}",
                        now_ms(), jitter_us
                    );
                    println!("{v}");
                    m.deadline_violations.push(v);
                }
            }

            // ── Lab 7: Consecutive miss counter (mirrors Lab 7 glitch counter) ─
            // Reset to 0 on successful insertion; increment on drop.
            // This is the same recovery logic pattern from Lab 7's run_sensor_loop().
            if accepted
            {
                m.consec_thermal_misses = 0;  // success → reset counter
            }
            else
            {
                m.total_dropped         += 1;
                m.consec_thermal_misses += 1;

                let fill = buffer.lock().unwrap().fill_ratio();
                let drop = format!(
                    "[{}ms] [DROP] Thermal seq={seq}  fill={:.1}%",
                    now_ms(), fill * 100.0
                );
                println!("{drop}");
                m.dropped_log.push(drop);

                // Safety alert after MAX_CONSEC_MISSES consecutive drops
                if m.consec_thermal_misses >= MAX_CONSEC_MISSES
                {
                    *emergency.lock().unwrap() = true;

                    let alert = format!(
                        "[{}ms] !!! SAFETY ALERT !!! {} thermal misses",
                        now_ms(), m.consec_thermal_misses
                    );
                    println!("{alert}");
                    m.safety_alerts.push(alert);

                    // serde: send a typed alert to GCS
                    let msg = encode(&OcsMsg::Alert
                    {
                        student: STUDENT_ID.into(),
                        event:   "thermal_alert".into(),
                        info:    AlertInfo
                        {
                            misses: Some(m.consec_thermal_misses),
                            ..Default::default()
                        },
                    });
                    let _ = udp_tx.send(msg);
                }
            }
        }

        // Transition to Degraded if buffer is >= 80% full
        if buffer.lock().unwrap().fill_ratio() >= DEGRADED_THRESHOLD
        {
            let mut s = state.lock().unwrap();
            if *s == SystemState::Normal
            {
                println!("[Thermal]  buf >= 80% → DEGRADED");
                *s = SystemState::Degraded;
            }
        }

        // Send telemetry to GCS every 5 readings to avoid flooding
        if seq % 5 == 0
        {
            let msg = encode(&OcsMsg::Thermal
            {
                student:  STUDENT_ID.into(),
                seq,
                temp,
                drift_ms,
            });
            let _ = udp_tx.send(msg);
        }

        seq += 1;
    }

    println!("[Thermal]  Exit.");
}


fn accelerometer_thread(
    buffer:  Arc<Mutex<PriorityBuffer>>,
    metrics: Arc<Mutex<SystemMetrics>>,
    udp_tx:  mpsc::Sender<String>,
    running: Arc<Mutex<bool>>,
)
{
    println!("[Accel]    period={}ms  buf_priority=2", ACCEL_PERIOD_MS);

    let mut seq:    u64     = 0;
    let task_start: Instant = Instant::now();

    while *running.lock().unwrap()
    {
        thread::sleep(Duration::from_millis(ACCEL_PERIOD_MS));

        // ── Jitter Calculation (Lab 2) ────────────────────────────────────────
        let expected_ms = seq * ACCEL_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Simulate a 3-axis accelerometer reading
        let ax  = (seq as f64 * 0.05).sin() * 0.10;
        let ay  = (seq as f64 * 0.07).cos() * 0.10;
        let az  = 9.81 + (seq as f64 * 0.03).sin() * 0.01;
        let mag = (ax * ax + ay * ay + az * az).sqrt();

        let reading = SensorReading
        {
            sensor_type:  SensorType::Accelerometer,
            value:        mag,
            timestamp_ms: now_ms(),
            priority:     2,   // lower priority than thermal
            sequence_num: seq,
        };

        // ── Lab 3: Insert latency measurement ────────────────────────────────
        let t0       = Instant::now();
        let accepted = buffer.lock().unwrap().push(reading);
        let ins_us   = t0.elapsed().as_micros() as u64;

        {
            let mut m = metrics.lock().unwrap();
            m.total_received    += 1;
            m.insert_latency_us.push(ins_us);
            m.drift_ms.push(drift_ms);

            if seq > 0
            {
                m.accel_jitter_us.push((drift_ms * 1_000).unsigned_abs() as i64);
            }

            if !accepted
            {
                m.total_dropped += 1;
                m.dropped_log.push(format!("[{}ms] [DROP] Accel seq={seq}", now_ms()));
            }
        }

        // Send telemetry every 10 readings
        if seq % 10 == 0
        {
            let msg = encode(&OcsMsg::Accel
            {
                student: STUDENT_ID.into(),
                seq,
                mag,
            });
            let _ = udp_tx.send(msg);
        }

        seq += 1;
    }

    println!("[Accel]    Exit.");
}


// =============================================================================
//  LAB 8 PART 1 — FRAGILE GYROSCOPE
//
//  This is exactly the "fragile_worker" pattern from Lab 8.
//  The gyroscope has a 3% random chance of panicking each iteration,
//  simulating a hardware fault. It runs in an infinite loop (no `running`
//  check) because the supervisor will detect the panic and restart it.
//
//  Lab 8 lesson: an isolated task panic does NOT crash the whole program
//  when spawned with thread::spawn. The supervisor catches it via
//  handle.join() returning Err(...).
// =============================================================================

fn fragile_gyroscope(
    generation: u32,                         // which restart generation this is
    buffer:     Arc<Mutex<PriorityBuffer>>,
    metrics:    Arc<Mutex<SystemMetrics>>,
    udp_tx:     mpsc::Sender<String>,
)
{
    println!("[Gyro-{generation}]  period={}ms  buf_priority=3", GYRO_PERIOD_MS);

    let mut seq:    u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        thread::sleep(Duration::from_millis(GYRO_PERIOD_MS));

        // ── Lab 8: 3% random panic (same as fragile_worker in Lab 8) ─────────
        if rand::random_range(0u32..100) < 3
        {
            println!("[Gyro-{generation}]  Hardware fault — panicking!");
            panic!("Gyroscope fault (gen {generation})");
            // The supervisor's handle.join() will return Err, triggering a restart
        }

        // ── Normal operation ──────────────────────────────────────────────────
        let expected_ms = seq * GYRO_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        let omega_z: f64 = 0.5 * (seq as f64 * 0.10).sin();

        let reading = SensorReading
        {
            sensor_type:  SensorType::Gyroscope,
            value:        omega_z,
            timestamp_ms: now_ms(),
            priority:     3,   // lowest priority sensor
            sequence_num: seq,
        };

        let t0       = Instant::now();
        let accepted = buffer.lock().unwrap().push(reading);
        let ins_us   = t0.elapsed().as_micros() as u64;

        {
            let mut m = metrics.lock().unwrap();
            m.total_received    += 1;
            m.insert_latency_us.push(ins_us);
            m.drift_ms.push(drift_ms);

            if seq > 0
            {
                m.gyro_jitter_us.push((drift_ms * 1_000).unsigned_abs() as i64);
            }

            if !accepted
            {
                m.total_dropped += 1;
                m.dropped_log.push(format!("[{}ms] [DROP] Gyro seq={seq}", now_ms()));
            }
        }

        if seq % 25 == 0
        {
            let msg = encode(&OcsMsg::Gyro
            {
                student: STUDENT_ID.into(),
                generation:     generation,
                seq,
                omega_z,
            });
            let _ = udp_tx.send(msg);
        }

        seq += 1;
    }
}


// =============================================================================
//  LAB 8 PART 2 — GYROSCOPE SUPERVISOR
//
//  This is the Supervisor Pattern from Lab 8.
//  It works exactly like the supervisor loop in Lab 8's main():
//
//    loop {
//        let handle = thread::spawn(fragile_worker);
//        match handle.join() {
//            Ok(_)  => break,          // normal exit
//            Err(_) => { restart... }  // panic → wait → retry
//        }
//    }
//
//  The supervisor never panics itself — it just keeps restarting the
//  gyroscope task until the program shuts down (running = false).
// =============================================================================

fn gyro_supervisor_thread(
    buffer:  Arc<Mutex<PriorityBuffer>>,
    metrics: Arc<Mutex<SystemMetrics>>,
    udp_tx:  mpsc::Sender<String>,
    running: Arc<Mutex<bool>>,
)
{
    println!("[Gyro-SUP] Supervisor online.");

    let mut generation: u32 = 0;

    while *running.lock().unwrap()
    {
        generation += 1;
        println!("[Gyro-SUP] Starting generation {generation}...");

        // Clone Arcs for the new child thread (Lab 2: Arc::clone)
        let b = Arc::clone(&buffer);
        let m = Arc::clone(&metrics);
        let t = udp_tx.clone();

        // Lab 8: spawn the fragile worker
        let handle = thread::spawn(move || fragile_gyroscope(generation, b, m, t));

        // Lab 8: match handle.join() to detect panic vs normal exit
        match handle.join()
        {
            Ok(_) =>
            {
                // Normal exit (shouldn't happen in loop{} — means running=false)
                println!("[Gyro-SUP] Normal exit — done.");
                return;
            }
            Err(_) =>
            {
                // Panic detected — same as Lab 8 supervisor restart logic
                metrics.lock().unwrap().gyro_restarts += 1;
                println!("[Gyro-SUP] Panic! Restarting in 1s...");

                // Notify GCS of the restart via serde-encoded alert
                let msg = encode(&OcsMsg::Alert
                {
                    student: STUDENT_ID.into(),
                    event:   "gyro_restart".into(),
                    info:    AlertInfo
                    {
                        generation: Some(generation),
                        ..Default::default()
                    },
                });
                let _ = udp_tx.send(msg);

                // Backoff before restarting (Lab 8: sleep between restarts)
                thread::sleep(Duration::from_secs(1));
            }
        }
    }

    println!("[Gyro-SUP] Exit.");
}


// =============================================================================
//  PART 2 — RM BACKGROUND TASKS via ScheduledThreadPool  (Lab 11)
//
//  Rate Monotonic (RM) scheduling: shortest period = highest priority.
//    health_monitor    200ms  RM P1 (highest)
//    data_compression  500ms  RM P2
//    antenna_alignment 1000ms RM P3 (lowest, preemptible)
//
//  These return closures (move ||) which are scheduled by a
//  ScheduledThreadPool — the same library used in Lab 11.
//
//  Each task captures its state (iter counter, Arcs) at creation time
//  via the `move` keyword — just like the thread closures in Lab 11.
// =============================================================================

// ── Health Monitor (RM P1) ────────────────────────────────────────────────────
// Reports buffer fill, system state, and drift every 200ms.
// Sends OcsMsg::Status telemetry to GCS via UDP.
fn make_health_task(
    buffer:  Arc<Mutex<PriorityBuffer>>,
    metrics: Arc<Mutex<SystemMetrics>>,
    state:   Arc<Mutex<SystemState>>,
    udp_tx:  mpsc::Sender<String>,
    running: Arc<Mutex<bool>>,
) -> impl FnMut() + Send + 'static
{
    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    // `move` captures iter, task_start, and all Arcs into the closure
    move ||
    {
        if !*running.lock().unwrap() { return; }

        let t0 = Instant::now();

        // Drift = how late is this task compared to its expected schedule
        let expected_ms = iter * HEALTH_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        let (fill, len) =
        {
            let b = buffer.lock().unwrap();
            (b.fill_ratio(), b.heap.len())
        };

        let sys_state = state.lock().unwrap().clone();

        println!(
            "[Health]   iter={iter:>4}  buf={len}/{BUFFER_CAPACITY} ({:.1}%)  \
             state={sys_state:?}  drift={drift_ms:+}ms",
            fill * 100.0
        );

        // Flag if health task itself is drifting (deadline violation)
        if drift_ms.abs() > 5
        {
            let v = format!(
                "[{}ms] [DEADLINE] Health drift={drift_ms:+}ms iter={iter}",
                now_ms()
            );
            println!("{v}");
            metrics.lock().unwrap().deadline_violations.push(v);
        }

        // serde: send status telemetry to GCS
        let msg = encode(&OcsMsg::Status
        {
            student:  STUDENT_ID.into(),
            iter,
            fill:     fill as f64 * 100.0,
            state:    format!("{sys_state:?}"),
            drift_ms,
        });
        let _ = udp_tx.send(msg);

        let active_ms = t0.elapsed().as_millis() as u64;

        {
            let mut m = metrics.lock().unwrap();
            m.drift_ms.push(drift_ms);
            m.active_ms  += active_ms;
            m.elapsed_ms  = task_start.elapsed().as_millis() as u64;
        }

        iter += 1;
    }
}

// ── Data Compression Task (RM P2) ─────────────────────────────────────────────
// Drains up to 20 readings from the priority buffer, "compresses" them
// (simulated as 50% size reduction), and enqueues a DataPacket for downlink.
fn make_compress_task(
    buffer:  Arc<Mutex<PriorityBuffer>>,
    dq:      Arc<Mutex<Vec<DataPacket>>>,  // downlink queue
    metrics: Arc<Mutex<SystemMetrics>>,
    running: Arc<Mutex<bool>>,
) -> impl FnMut() + Send + 'static
{
    let mut pkt_id: u64     = 0;
    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    move ||
    {
        if !*running.lock().unwrap() { return; }

        let t0          = Instant::now();
        let expected_ms = iter * COMPRESS_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // ── Lab 11: drain up to 20 items with .pop() (priority order) ─────────
        let mut batch: Vec<SensorReading> = Vec::new();
        {
            let mut buf = buffer.lock().unwrap();
            for _ in 0..20
            {
                match buf.pop()
                {
                    Some(r) => batch.push(r),
                    None    => break,
                }
            }
        }

        if !batch.is_empty()
        {
            // serde_json::json! macro builds a structured batch payload
            let data_array: Vec<serde_json::Value> = batch.iter()
                .map(|r| serde_json::json!(
                {
                    "s": format!("{:?}", r.sensor_type),
                    "v": r.value,
                    "t": r.timestamp_ms,
                }))
                .collect();

            let raw = serde_json::json!(
            {
                "pkt":  pkt_id,
                "n":    batch.len(),
                "ts":   now_ms(),
                "data": data_array,
            }).to_string();

            // Simulate ~50% compression ratio
            let size = (raw.len() / 2).max(1);

            // Lab 3: measure queue insert latency
            let t1 = Instant::now();
            dq.lock().unwrap().push(DataPacket
            {
                packet_id:     pkt_id,
                payload:       raw,
                created_at_ms: now_ms(),
                size_bytes:    size,
            });
            let q_lat_us = t1.elapsed().as_micros() as u64;

            println!(
                "[Compress] pkt={pkt_id}  n={}  {}B  q_lat={}µs  drift={drift_ms:+}ms",
                batch.len(), size, q_lat_us
            );

            pkt_id += 1;
        }

        if drift_ms.abs() > 10
        {
            let v = format!(
                "[{}ms] [DEADLINE] Compress drift={drift_ms:+}ms iter={iter}",
                now_ms()
            );
            println!("{v}");
            metrics.lock().unwrap().deadline_violations.push(v);
        }

        metrics.lock().unwrap().drift_ms.push(drift_ms);
        metrics.lock().unwrap().active_ms += t0.elapsed().as_millis() as u64;

        iter += 1;
    }
}

// ── Antenna Alignment Task (RM P3) ────────────────────────────────────────────
// Lowest priority RM task. Can be preempted (skipped) when emergency=true
// or system is in MissionAbort state — demonstrating priority-based preemption.
fn make_antenna_task(
    metrics:   Arc<Mutex<SystemMetrics>>,
    state:     Arc<Mutex<SystemState>>,
    emergency: Arc<Mutex<bool>>,
    running:   Arc<Mutex<bool>>,
) -> impl FnMut() + Send + 'static
{
    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    move ||
    {
        if !*running.lock().unwrap() { return; }

        // ── Preemption check: skip if emergency or abort ───────────────────────
        // This simulates RM preemption — higher-priority work takes over.
        if *emergency.lock().unwrap() || *state.lock().unwrap() == SystemState::MissionAbort
        {
            println!("[Antenna]  iter={iter:>4}  PREEMPTED at {}ms", now_ms());
            let v = format!("[{}ms] [PREEMPT] Antenna iter={iter}", now_ms());
            metrics.lock().unwrap().deadline_violations.push(v);
            iter += 1;
            return;
        }

        let t0          = Instant::now();
        let expected_ms = iter * ANTENNA_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Compute azimuth and elevation for this iteration
        let az = (iter as f64 * 7.2) % 360.0;
        let el = 30.0 + 20.0 * (iter as f64 * 0.05).sin();

        println!(
            "[Antenna]  iter={iter:>4}  az={az:>6.1}deg  el={el:>5.1}deg  drift={drift_ms:+}ms"
        );

        if drift_ms.abs() > 20
        {
            let v = format!(
                "[{}ms] [DEADLINE] Antenna drift={drift_ms:+}ms iter={iter}",
                now_ms()
            );
            println!("{v}");
            metrics.lock().unwrap().deadline_violations.push(v);
        }

        metrics.lock().unwrap().drift_ms.push(drift_ms);
        metrics.lock().unwrap().active_ms += t0.elapsed().as_millis() as u64;

        iter += 1;
    }
}


// =============================================================================
//  PART 3 — DOWNLINK THREAD  (Radio typestate from Lab 7)
//
//  Every VISIBILITY_INTERVAL_S seconds, the satellite is "visible" to
//  ground and can transmit queued data. The downlink uses the Radio
//  typestate (Lab 7) to enforce the sequence:
//    Radio<Idle> → .initialise() → Radio<Transmitting> → .transmit() → Radio<Idle>
//
//  You cannot skip initialise() — the type system prevents it.
// =============================================================================

fn downlink_thread(
    dq:      Arc<Mutex<Vec<DataPacket>>>,
    metrics: Arc<Mutex<SystemMetrics>>,
    state:   Arc<Mutex<SystemState>>,
    udp_tx:  mpsc::Sender<String>,
    running: Arc<Mutex<bool>>,
)
{
    println!(
        "[Downlink] visibility={}s  window={}ms  (Radio typestate active)",
        VISIBILITY_INTERVAL_S, DOWNLINK_WINDOW_MS
    );

    while *running.lock().unwrap()
    {
        // Wait for next visibility window
        thread::sleep(Duration::from_secs(VISIBILITY_INTERVAL_S));
        if !*running.lock().unwrap() { break; }

        println!("\n[Downlink] === Visibility window at {}ms ===", now_ms());

        // Check if the downlink queue is overflowing → Degraded
        let q_fill = dq.lock().unwrap().len() as f32 / BUFFER_CAPACITY as f32;
        if q_fill >= DEGRADED_THRESHOLD
        {
            let mut s = state.lock().unwrap();
            if *s == SystemState::Normal
            {
                println!("[Downlink] Queue >= 80% → DEGRADED");
                *s = SystemState::Degraded;
            }
        }

        // ── Lab 7: Typestate transition — Radio<Idle> → Radio<Transmitting> ───
        let radio = Radio::<Idle>::new();

        match radio.initialise()
        {
            None =>
            {
                // Initialisation failed (exceeded 5ms deadline)
                let v = format!(
                    "[{}ms] [DEADLINE] Radio init exceeded {}ms",
                    now_ms(), DOWNLINK_INIT_LIMIT_MS
                );
                metrics.lock().unwrap().deadline_violations.push(v);
            }
            Some(tx_radio) =>
            {
                // Drain all queued packets for transmission
                let packets: Vec<DataPacket> = dq.lock().unwrap().drain(..).collect();

                if packets.is_empty()
                {
                    println!("[Downlink] Nothing queued.");
                }

                // transmit() consumes tx_radio (Radio<Transmitting>)
                // and returns Radio<Idle> — state machine enforced at compile time
                let _idle = tx_radio.transmit(&packets, &metrics, &udp_tx);
            }
        }
    }

    println!("[Downlink] Exit.");
}


// =============================================================================
//  PART 4 — FAULT INJECTOR  (Lab 7: enum FaultType + Lab 3: recovery timing)
//
//  Periodically injects simulated hardware faults to test the system's
//  recovery behaviour. Uses Lab 7's enum + match pattern to classify
//  each fault and apply the appropriate response.
//
//  Recovery time is measured with Instant::now() (Lab 3).
//  If recovery takes longer than RECOVERY_LIMIT_MS → MissionAbort.
// =============================================================================

fn fault_injector_thread(
    metrics:   Arc<Mutex<SystemMetrics>>,
    state:     Arc<Mutex<SystemState>>,
    emergency: Arc<Mutex<bool>>,
    udp_tx:    mpsc::Sender<String>,
    running:   Arc<Mutex<bool>>,
)
{
    println!(
        "[Faults]   interval={}s  recovery_limit={}ms",
        FAULT_INTERVAL_S, RECOVERY_LIMIT_MS
    );

    let mut count: u32 = 0;

    while *running.lock().unwrap()
    {
        thread::sleep(Duration::from_secs(FAULT_INTERVAL_S));
        if !*running.lock().unwrap() { break; }

        count += 1;

        // ── Lab 7: random fault selection, same enum+match pattern ────────────
        let fault = match rand::random_range(0u32..3)
        {
            0 => FaultType::SensorBusHang(80),
            1 => FaultType::CorruptedReading,
            _ => FaultType::PowerSpike,
        };

        let fmsg = format!("[{}ms] [FAULT #{count}] {fault:?}", now_ms());
        println!("\n{fmsg}");
        metrics.lock().unwrap().fault_log.push(fmsg);

        // serde: alert the GCS of the fault
        let msg = encode(&OcsMsg::Alert
        {
            student: STUDENT_ID.into(),
            event:   "fault".into(),
            info:    AlertInfo
            {
                count:      Some(count),
                fault_type: Some(format!("{fault:?}")),
                ..Default::default()
            },
        });
        let _ = udp_tx.send(msg);

        // ── Lab 7: match on fault variant to decide recovery action ───────────
        match fault
        {
            FaultType::SensorBusHang(ms) =>
            {
                println!("[Faults]   {ms}ms bus hang...");
                thread::sleep(Duration::from_millis(ms));
            }
            FaultType::CorruptedReading =>
            {
                println!("[Faults]   Corrupted reading injected");
            }
            FaultType::PowerSpike =>
            {
                println!("[Faults]   Power spike → DEGRADED + emergency");
                *state.lock().unwrap()     = SystemState::Degraded;
                *emergency.lock().unwrap() = true;
            }
        }

        // ── Lab 3: Measure recovery time ─────────────────────────────────────
        let t0 = Instant::now();
        println!("[Faults]   Recovering...");
        thread::sleep(Duration::from_millis(50));  // simulate recovery work

        // Clear fault state
        *emergency.lock().unwrap() = false;
        *state.lock().unwrap()     = SystemState::Normal;

        let rec_ms = t0.elapsed().as_millis() as u64;
        println!("[Faults]   Recovery in {rec_ms}ms");

        {
            let mut m = metrics.lock().unwrap();
            m.recovery_times_ms.push(rec_ms);

            if rec_ms > RECOVERY_LIMIT_MS
            {
                let abort = format!(
                    "[{}ms] !!! MISSION ABORT !!! recovery {rec_ms}ms",
                    now_ms()
                );
                println!("{abort}");
                m.safety_alerts.push(abort);
                *state.lock().unwrap() = SystemState::MissionAbort;
            }
        }
    }

    println!("[Faults]   Exit.");
}


// =============================================================================
//  METRICS REPORTER THREAD
//
//  Prints a formatted summary report every 10 seconds.
//  Runs as its own OS thread so it doesn't interfere with sensor threads.
// =============================================================================

fn metrics_reporter_thread(metrics: Arc<Mutex<SystemMetrics>>, running: Arc<Mutex<bool>>)
{
    while *running.lock().unwrap()
    {
        thread::sleep(Duration::from_secs(10));
        if !*running.lock().unwrap() { break; }

        print_report(&metrics.lock().unwrap());
    }
}

fn print_report(m: &SystemMetrics)
{
    let loss = if m.total_received > 0
    {
        m.total_dropped as f64 / m.total_received as f64 * 100.0
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
    println!( "║  OCS REPORT  at {}ms", now_ms());
    println!( "╠══════════════════════════════════════════════════════╣");
    println!(
        "║  Rx: {}  Dropped: {} ({:.2}%)  Gyro restarts: {}",
        m.total_received, m.total_dropped, loss, m.gyro_restarts
    );
    println!("║  Jitter (µs)  limit={}µs", JITTER_LIMIT_US);
    print_stat_row("Thermal [CRITICAL]", &m.thermal_jitter_us);
    print_stat_row("Accelerometer",      &m.accel_jitter_us);
    print_stat_row("Gyroscope",          &m.gyro_jitter_us);
    println!("║  Drift (ms)");
    print_stat_row("All tasks", &m.drift_ms);
    println!("║  Insert latency (µs)");
    let lat: Vec<i64> = m.insert_latency_us.iter().map(|&v| v as i64).collect();
    print_stat_row("buffer.push()", &lat);
    println!("║  Deadline violations: {}", m.deadline_violations.len());
    for v in m.deadline_violations.iter().take(3)
    {
        println!("║    {v}");
    }
    if m.deadline_violations.len() > 3
    {
        println!("║    ... and {} more", m.deadline_violations.len() - 3);
    }
    println!("║  Faults: {}", m.fault_log.len());
    for f in &m.fault_log
    {
        println!("║    {f}");
    }
    if !m.recovery_times_ms.is_empty()
    {
        let max_r = m.recovery_times_ms.iter().max().unwrap();
        let avg_r = m.recovery_times_ms.iter().sum::<u64>() as f64
                    / m.recovery_times_ms.len() as f64;
        println!("║    recovery max={max_r}ms  avg={avg_r:.1}ms");
    }
    println!("║  CPU ≈ {cpu:.2}%");
    if !m.safety_alerts.is_empty()
    {
        println!("║  SAFETY ALERTS:");
        for a in &m.safety_alerts
        {
            println!("║    {a}");
        }
    }
    println!("╚══════════════════════════════════════════════════════╝\n");
}


// =============================================================================
//  MAIN FUNCTION
//
//  Sets up all shared state, spawns all OS threads, schedules RM background
//  tasks via ScheduledThreadPool (Lab 11), then waits for SIM_DURATION_S
//  before shutting everything down gracefully.
// =============================================================================

fn main()
{
    println!("╔═══════════════════════════════════════════════════════╗");
    println!("║  OCS — Satellite Onboard Control System               ║");
    println!("║  CT087-3-3  |  Student A  |  Hard RTS / OS threads    ║");
    println!("╚═══════════════════════════════════════════════════════╝\n");

    // ── Shared State Setup ────────────────────────────────────────────────────
    // Lab 2: Arc<Mutex<T>> allows multiple threads to safely share ownership.
    // Arc  = Atomic Reference Counting (shared ownership across threads)
    // Mutex = Mutual Exclusion (only one thread can access at a time)
    let buffer    = Arc::new(Mutex::new(PriorityBuffer::new(BUFFER_CAPACITY)));
    let metrics   = Arc::new(Mutex::new(SystemMetrics::new()));  // Lab 3: with_capacity
    let state     = Arc::new(Mutex::new(SystemState::Normal));
    let dq        = Arc::new(Mutex::new(Vec::<DataPacket>::with_capacity(50)));
    let emergency = Arc::new(Mutex::new(false));
    let running   = Arc::new(Mutex::new(true));

    // ── Lab 11: mpsc channel for UDP telemetry ────────────────────────────────
    // mpsc = Multi-Producer, Single-Consumer
    // Multiple sensor threads send strings → one udp_sender_thread consumes them
    let (udp_tx, udp_rx) = mpsc::channel::<String>();

    println!("[OCS] Config:");
    println!("      Telemetry → GCS : {GCS_TELEM_ADDR}  (serde_json encoded)");
    println!("      Commands  ← GCS : {OCS_CMD_BIND}   (serde_json decoded)");
    println!("      Buffer capacity : {BUFFER_CAPACITY}");
    println!("      Radio typestate : Idle → Transmitting (Lab 7 PhantomData)");
    println!("      RM pool tasks   : health/compress/antenna via ScheduledThreadPool (Lab 11)");
    println!("      Sim duration    : {SIM_DURATION_S}s\n");

    // ── Thread Handles (Lab 2: thread::spawn) ────────────────────────────────
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();

    // UDP sender thread — consumes the mpsc channel and fires packets (Lab 9 + Lab 11)
    handles.push(thread::spawn(
    {
        let rx = udp_rx;
        move || udp_sender_thread(rx)
    }));

    // Command receiver thread — listens for GCS commands over UDP (Lab 9)
    handles.push(thread::spawn(
    {
        let r = Arc::clone(&running);
        move || command_receiver_thread(r)
    }));

    // Thermal sensor thread — highest priority, safety-critical (Lab 2 + Lab 7)
    handles.push(thread::spawn(
    {
        let (b, m, s, e, t, r) = (
            Arc::clone(&buffer), Arc::clone(&metrics), Arc::clone(&state),
            Arc::clone(&emergency), udp_tx.clone(), Arc::clone(&running),
        );
        move || thermal_sensor_thread(b, m, s, e, t, r)
    }));

    // Accelerometer thread (Lab 2)
    handles.push(thread::spawn(
    {
        let (b, m, t, r) = (
            Arc::clone(&buffer), Arc::clone(&metrics),
            udp_tx.clone(), Arc::clone(&running),
        );
        move || accelerometer_thread(b, m, t, r)
    }));

    // Gyroscope supervisor — monitors and restarts the fragile gyro (Lab 8)
    handles.push(thread::spawn(
    {
        let (b, m, t, r) = (
            Arc::clone(&buffer), Arc::clone(&metrics),
            udp_tx.clone(), Arc::clone(&running),
        );
        move || gyro_supervisor_thread(b, m, t, r)
    }));

    // ── Lab 11: ScheduledThreadPool for RM background tasks ──────────────────
    // Uses execute_at_fixed_rate(initial_delay, period, closure)
    // This is the same concept as the scheduled executor in Lab 11.
    let rm_pool = ScheduledThreadPool::new(3);

    rm_pool.execute_at_fixed_rate(
        Duration::from_millis(10),
        Duration::from_millis(HEALTH_PERIOD_MS),
        make_health_task(
            Arc::clone(&buffer), Arc::clone(&metrics),
            Arc::clone(&state), udp_tx.clone(), Arc::clone(&running),
        ),
    );

    rm_pool.execute_at_fixed_rate(
        Duration::from_millis(20),
        Duration::from_millis(COMPRESS_PERIOD_MS),
        make_compress_task(
            Arc::clone(&buffer), Arc::clone(&dq),
            Arc::clone(&metrics), Arc::clone(&running),
        ),
    );

    rm_pool.execute_at_fixed_rate(
        Duration::from_millis(30),
        Duration::from_millis(ANTENNA_PERIOD_MS),
        make_antenna_task(
            Arc::clone(&metrics), Arc::clone(&state),
            Arc::clone(&emergency), Arc::clone(&running),
        ),
    );

    // Downlink thread — uses Radio typestate (Lab 7)
    handles.push(thread::spawn(
    {
        let (d, m, s, t, r) = (
            Arc::clone(&dq), Arc::clone(&metrics), Arc::clone(&state),
            udp_tx.clone(), Arc::clone(&running),
        );
        move || downlink_thread(d, m, s, t, r)
    }));

    // Fault injector thread (Lab 7 enum + Lab 3 timing)
    handles.push(thread::spawn(
    {
        let (m, s, e, t, r) = (
            Arc::clone(&metrics), Arc::clone(&state), Arc::clone(&emergency),
            udp_tx.clone(), Arc::clone(&running),
        );
        move || fault_injector_thread(m, s, e, t, r)
    }));

    // Periodic metrics reporter
    handles.push(thread::spawn(
    {
        let (m, r) = (Arc::clone(&metrics), Arc::clone(&running));
        move || metrics_reporter_thread(m, r)
    }));

    // ── Lab 11: drop the last sender so udp_sender_thread knows when to exit ──
    // When all senders are dropped, "for msg in rx" in udp_sender_thread returns.
    drop(udp_tx);

    println!(
        "[OCS] {} OS threads + 3 ScheduledThreadPool tasks online.  Running for {SIM_DURATION_S}s...\n",
        handles.len()
    );

    // ── Run for the simulation duration ──────────────────────────────────────
    thread::sleep(Duration::from_secs(SIM_DURATION_S));

    // ── Graceful Shutdown ─────────────────────────────────────────────────────
    println!("\n[OCS] Simulation ended — signalling shutdown...");
    *running.lock().unwrap() = false;

    // Dropping the pool joins the pool threads automatically (Lab 11)
    drop(rm_pool);

    // Wait for all OS threads to finish (Lab 2: handle.join())
    for h in handles
    {
        let _ = h.join();
    }

    // Print final summary
    println!("\n[OCS] FINAL REPORT:");
    print_report(&metrics.lock().unwrap());
    println!("[OCS] Done.");
}