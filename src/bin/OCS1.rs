// ── Standard Library Imports ─────────────────────────────────────────────────
use std::collections::BinaryHeap;   // for priority queue (Lab 11)
use std::fs::{File, OpenOptions};   // for runtime log file
use std::io::Write;                 // for writing log lines
use std::marker::PhantomData;       // for typestate pattern (Lab 7)
use std::net::UdpSocket;            // for blocking UDP socket (Lab 9)
use std::sync::{Arc, Mutex};        // for shared ownership across threads (Lab 2)
use std::sync::mpsc;                // for message passing between threads (Lab 11)
use std::thread;                    // for OS thread creation (Lab 2)
use std::time::{Duration, Instant};
 
// ── External Crate Imports ────────────────────────────────────────────────────
use rand;                                   // random number generation (Lab 8 fault simulation)
use scheduled_thread_pool::ScheduledThreadPool; // thread pool for RM tasks (Lab 11)
use serde::{Deserialize, Serialize};        // automatic JSON serialisation/deserialisation
use serde_json;                             // JSON encode/decode for UDP payloads
 
 
// =============================================================================
//  PART 0 — CONSTANTS   (Lab 1: const keyword)
//
//  Constants are declared with `const` instead of `let`.
//  They must have an explicit type and cannot be mutable.
//  They live for the entire program lifetime (no ownership).
// =============================================================================
 
// ── Sensor Sampling Periods (milliseconds) ────────────────────────────────────
const THERMAL_SENSOR_PERIOD: u64 = 50;         // highest priority sensor, fastest rate
const ACCELEROMETER_PERIOD: u64 = 120;
const GYROSCOPE_PERIOD: u64 = 333;
 
// ── Background Task Periods (milliseconds) ────────────────────────────────────
const HEALTH_MONITOR_PERIOD: u64 = 200;
const DATA_COMPRESSION_PERIOD: u64 = 500;
const ANTENNA_ALIGNMENT_PERIOD: u64 = 1000;
 
// ── Timing and Safety Thresholds ─────────────────────────────────────────────
const JITTER_WARNING_LIMIT_MICROSECONDS: i64 = 1000;  // warn if jitter exceeds 1ms
const MAX_CONSECUTIVE_THERMAL_MISSES: u32 = 3;        // safety alert after 3 consecutive drops
const PRIORITY_BUFFER_CAPACITY: usize = 100;          // max items in the priority buffer
const DEGRADED_MODE_THRESHOLD: f32 = 0.80;            // enter degraded mode at 80% full
 
// ── Downlink / Radio Parameters ───────────────────────────────────────────────
const VISIBILITY_WINDOW_INTERVAL: u64 = 10;           // seconds
const DOWNLINK_WINDOW_DURATION: u64 = 30;             // milliseconds
const DOWNLINK_INITIALISATION_DEADLINE: u64 = 5;      // milliseconds
 
// ── Fault Injection and Simulation ───────────────────────────────────────────
const FAULT_INJECTION_INTERVAL: u64 = 60;             // seconds
const RECOVERY_TIME_LIMIT: u64 = 200;                // milliseconds
const SIMULATION_DURATION_SECONDS: u64 = 180;        // 180 seconds = 3 minutes
 
// ── Lab 3: Pre-allocated capacities (no runtime realloc during simulation) ────
// We call Vec::with_capacity() at startup so the Vec never needs to grow
// during the simulation, avoiding unpredictable heap allocation latency.
const MAX_JITTER_SAMPLES: usize = 10000;
const MAX_DRIFT_SAMPLES: usize = 20000;
const MAX_LATENCY_SAMPLES: usize = 20000;
const MAX_LOG_ENTRIES: usize = 500;
 
// ── Network Addresses ─────────────────────────────────────────────────────────
const GCS_TELEMETRY_ADDRESS: &str = "127.0.0.1:9000";
const OCS_COMMAND_BIND_ADDRESS: &str = "0.0.0.0:9001";
const UDP_READ_TIMEOUT: u64 = 100;
 
// ── Runtime Log File ──────────────────────────────────────────────────────────
const RUNTIME_LOG_FILE: &str = "ocs_runtime.log";
 
 
// =============================================================================
//  SERDE MESSAGE TYPES
//
//  Every UDP payload the OCS sends is one of these enum variants.
//  #[serde(tag = "tag")] writes {"tag":"thermal",...} on the wire —
//  the same format as the old manual format! strings, but now the compiler
//  checks every field name and type at compile time.
//
//  The GCS deserialises with serde_json::from_str::<OcsMessage>(&payload),
//  matching on the same enum — no more payload.contains("\"tag\":\"alert\"").
// =============================================================================
 
// The main message enum — each variant maps to a different telemetry type
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "tag", rename_all = "snake_case")]
enum OcsMessage
{
    Thermal
    {
        seq: u64,
        temp: f64,
        drift_ms: i64,
    },
 
    Accel
    {
        seq: u64,
        mag: f64,
    },
 
    Gyro
    {
        generation: u32,
        seq: u64,
        omega_z: f64,
    },
 
    Status
    {
        iter: u64,
        fill: f64,
        state: String,
        drift_ms: i64,
    },
 
    Downlink
    {
        pkt: u64,
        bytes: usize,
        q_lat_ms: u64,
    },
 
    // Alert uses #[serde(flatten)] so AlertInfo fields appear directly
    // in the JSON object alongside "student" and "event", rather than nested.
    Alert
    {
        event: String,
        #[serde(flatten)]
        info: AlertInfo,
    },
}
 
// Optional extra fields for Alert messages.
// We use Option<T> so absent fields are omitted (skip_serializing_if) rather
// than written as null — keeps the wire format clean.
#[derive(Serialize, Deserialize, Debug, Default)]
struct AlertInfo
{
    #[serde(skip_serializing_if = "Option::is_none")]
    pub misses: Option<u32>,     // for thermal miss alerts
 
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>, // for gyro restart alerts
 
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,      // fault occurrence count
 
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault_type: Option<String>, // e.g. "SensorBusHang", "PowerSpike"
}
 
// Command messages arriving FROM the GCS. The OCS only needs tag/cmd/ts.
#[derive(Serialize, Deserialize, Debug)]
struct GcsCommand
{
    pub tag: String,      // always "cmd"
    pub student: String,
    pub cmd: String,      // e.g. "ThermalCheck", "EmergencyHalt"
    pub ts: u64,          // simulation timestamp in ms (or sender-side timestamp)
}
 
// Helper: serialise an OcsMessage to a JSON string.
// If encoding fails (shouldn't happen), logs the error and returns "".
fn encode_message(message: &OcsMessage) -> String
{
    serde_json::to_string(message)
        .unwrap_or_else(|error|
        {
            println!("[ENCODE] {error}");
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
    sensor_type: SensorType,
    value: f64,
    timestamp_ms: u64,   // simulation-relative time in ms
    priority: u8,        // lower number = higher priority (1 is most critical)
}
 
// ── BinaryHeap ordering for SensorReading ────────────────────────────────────
// Rust's BinaryHeap is a MAX-heap by default.
// We flip the comparison so that LOWER priority numbers come out first
// (i.e. priority 1 = thermal = most urgent pops before priority 3 = gyro).
impl PartialEq for SensorReading
{
    fn eq(&self, other: &Self) -> bool
    {
        self.priority == other.priority
    }
}
impl Eq for SensorReading {}
 
impl PartialOrd for SensorReading
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering>
    {
        Some(self.cmp(other))
    }
}
 
impl Ord for SensorReading
{
    // Reverse the comparison: lower priority number → higher heap position
    fn cmp(&self, other: &Self) -> std::cmp::Ordering
    {
        other.priority.cmp(&self.priority)
    }
}
 
// A compressed packet ready for downlink transmission
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DataPacket
{
    packet_id: u64,
    payload: String,      // serde_json batch of sensor readings
    created_at_ms: u64,   // simulation-relative time when packet was queued
    size_bytes: usize,
}
 
// Overall system health state (Lab 1: enum)
#[derive(Debug, Clone, PartialEq)]
enum SystemState
{
    Normal,
    Degraded,
    MissionAbort,
}
 
// Types of faults the injector can simulate (Lab 7: enum FaultType)
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
 
// Methods available ONLY when the radio is Idle
impl Radio<Idle>
{
    // Create a new radio, always starts Idle
    fn new() -> Self
    {
        Radio { _state: PhantomData }
    }
 
    // Idle → Transmitting.
    // Returns None if hardware init exceeds the 5ms deadline (hard deadline).
    fn initialise(
        self,
        simulation_start_time: &Instant,
        runtime_logger: &Shared<File>,
    ) -> Option<Radio<Transmitting>>
    {
        let initialisation_start_time = Instant::now();
        thread::sleep(Duration::from_millis(3));  // simulate hardware init delay
 
        let initialisation_duration_ms = initialisation_start_time.elapsed().as_millis() as u64;
 
        if initialisation_duration_ms > DOWNLINK_INITIALISATION_DEADLINE
        {
            let log_line = format!(
                "[{}ms] [RADIO] Initialisation {}ms > {}ms — staying Idle",
                simulation_elapsed_ms(simulation_start_time),
                initialisation_duration_ms,
                DOWNLINK_INITIALISATION_DEADLINE
            );
 
            // Deadline violation — always shown on terminal
            print_and_log_important(runtime_logger, &log_line);
 
            None
        }
        else
        {
            let log_line = format!(
                "[{}ms] [RADIO] Initialisation OK ({}ms) — Transmitting",
                simulation_elapsed_ms(simulation_start_time),
                initialisation_duration_ms
            );
 
            write_runtime_log(runtime_logger, &log_line);
 
            Some(Radio { _state: PhantomData })
        }
    }
}
 
// Methods available ONLY when the radio is Transmitting
impl Radio<Transmitting>
{
    // Transmit all queued packets, then return to Idle state.
    // Note: self is consumed (moved), enforcing the state transition.
    fn transmit(
        self,
        packets: &[DataPacket],
        metrics: &Shared<SystemMetrics>,
        udp_sender: &mpsc::Sender<String>,
        simulation_start_time: &Instant,
        runtime_logger: &Shared<File>,
    ) -> Radio<Idle>
    {
        let transmission_start_time = Instant::now();
        let mut transmitted_packet_count = 0usize;
        let mut total_transmitted_bytes = 0usize;
 
        for packet in packets
        {
            // Hard deadline: we must finish within the downlink window
            if transmission_start_time.elapsed().as_millis() as u64 >= DOWNLINK_WINDOW_DURATION
            {
                let violation = format!(
                    "[{}ms] [DEADLINE] Downlink window exceeded after {} packets",
                    simulation_elapsed_ms(simulation_start_time),
                    transmitted_packet_count
                );
 
                println!("{violation}");
                write_runtime_log(runtime_logger, &violation);
                log_deadline_violation(metrics, violation);
                break;
            }
 
            // Queue latency = time since this packet was created
            let queue_latency_ms = simulation_elapsed_ms(simulation_start_time)
                .saturating_sub(packet.created_at_ms);
 
            total_transmitted_bytes += packet.size_bytes;
            transmitted_packet_count += 1;
 
            let log_line = format!(
                "[{}ms] [DOWNLINK] TX packet={}  {}B  q_lat={}ms",
                simulation_elapsed_ms(simulation_start_time),
                packet.packet_id,
                packet.size_bytes,
                queue_latency_ms
            );
            write_runtime_log(runtime_logger, &log_line);
 
            // serde: send downlink metadata to GCS via UDP
            send_ocs_message(
                udp_sender,
                OcsMessage::Downlink
                {
                    pkt: packet.packet_id,
                    bytes: packet.size_bytes,
                    q_lat_ms: queue_latency_ms,
                }
            );
        }
 
        let transmission_duration_ms = transmission_start_time.elapsed().as_millis() as u64;
 
        let summary_line = format!(
            "[{}ms] [DOWNLINK] Done  {}/{} packets  {}B  {}ms",
            simulation_elapsed_ms(simulation_start_time),
            transmitted_packet_count,
            packets.len(),
            total_transmitted_bytes,
            transmission_duration_ms
        );
 
        println!("{summary_line}");
        write_runtime_log(runtime_logger, &summary_line);
 
        // Return the Idle state — caller now holds Radio<Idle>
        Radio { _state: PhantomData }
    }
}
 
 
// =============================================================================
//  SHARED METRICS  (Lab 3: Vec::with_capacity, no runtime realloc)
// =============================================================================
 
struct SystemMetrics
{
    // Jitter samples per sensor (Lab 2: jitter = |actual - expected|)
    thermal_jitter_microseconds: Vec<i64>,
    accelerometer_jitter_microseconds: Vec<i64>,
    gyroscope_jitter_microseconds: Vec<i64>,
 
    // Task scheduling drift and buffer insert latency (Lab 3)
    drift_milliseconds: Vec<i64>,
    insert_latency_microseconds: Vec<u64>,
 
    // Recovery and fault tracking
    recovery_times_milliseconds: Vec<u64>,
    dropped_log: Vec<String>,
    deadline_violations: Vec<String>,
    fault_log: Vec<String>,
    safety_alerts: Vec<String>,
 
    // Counters
    total_received_readings: u64,
    total_dropped_readings: u64,
    consecutive_thermal_misses: u32,
    active_time_milliseconds: u64,
    elapsed_time_milliseconds: u64,
    gyroscope_restart_count: u32,
}
 
impl SystemMetrics
{
    fn new() -> Self
    {
        SystemMetrics
        {
            thermal_jitter_microseconds: Vec::with_capacity(MAX_JITTER_SAMPLES),
            accelerometer_jitter_microseconds: Vec::with_capacity(MAX_JITTER_SAMPLES),
            gyroscope_jitter_microseconds: Vec::with_capacity(MAX_JITTER_SAMPLES),
 
            drift_milliseconds: Vec::with_capacity(MAX_DRIFT_SAMPLES),
            insert_latency_microseconds: Vec::with_capacity(MAX_LATENCY_SAMPLES),
 
            recovery_times_milliseconds: Vec::with_capacity(10),
            dropped_log: Vec::with_capacity(MAX_LOG_ENTRIES),
            deadline_violations: Vec::with_capacity(MAX_LOG_ENTRIES),
            fault_log: Vec::with_capacity(MAX_LOG_ENTRIES),
            safety_alerts: Vec::with_capacity(MAX_LOG_ENTRIES),
 
            total_received_readings: 0,
            total_dropped_readings: 0,
            consecutive_thermal_misses: 0,
            active_time_milliseconds: 0,
            elapsed_time_milliseconds: 0,
            gyroscope_restart_count: 0,
        }
    }
}
 
 
// =============================================================================
//  BOUNDED PRIORITY BUFFER  (Lab 11 concept upgrade)
// =============================================================================
 
struct PriorityBuffer
{
    heap: BinaryHeap<SensorReading>,  // max-heap with reversed ordering
    capacity: usize,
}
 
impl PriorityBuffer
{
    fn new(capacity: usize) -> Self
    {
        PriorityBuffer
        {
            heap: BinaryHeap::with_capacity(capacity),
            capacity,
        }
    }
 
    // Push a reading. Returns false (drop) if buffer is at capacity.
    fn push(&mut self, reading: SensorReading) -> bool
    {
        if self.heap.len() >= self.capacity
        {
            return false;
        }
 
        self.heap.push(reading);
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
 
// Shared Arc<Mutex<T>> alias to reduce signature noise
type Shared<T> = Arc<Mutex<T>>;
 
// Returns simulation-relative time in milliseconds from simulation start
fn simulation_elapsed_ms(simulation_start_time: &Instant) -> u64
{
    simulation_start_time.elapsed().as_millis() as u64
}
 
// Returns whether the system is still running
fn is_running(running: &Shared<bool>) -> bool
{
    *running.lock().unwrap()
}
 
// Helper: serialise and send a typed OcsMessage through the UDP sender channel
fn send_ocs_message(udp_sender: &mpsc::Sender<String>, message: OcsMessage)
{
    let encoded_message = encode_message(&message);
    let _ = udp_sender.send(encoded_message);
}
 
// Helper: push a deadline violation into metrics
fn log_deadline_violation(metrics: &Shared<SystemMetrics>, violation: String)
{
    metrics.lock().unwrap().deadline_violations.push(violation);
}
 
// Helper: get buffer fill ratio
fn priority_buffer_fill_ratio(priority_buffer: &Shared<PriorityBuffer>) -> f32
{
    priority_buffer.lock().unwrap().fill_ratio()
}
 
// Helper: calculate scheduling drift in ms
//
// IMPORTANT:
// This now correctly uses simulation-relative time from the common simulation
// start instant, rather than task-local elapsed time from each task's own
// Instant. That gives a consistent timeline for the entire simulation.
fn calculate_drift_milliseconds(
    simulation_start_time: &Instant,
    expected_elapsed_milliseconds: u64,
) -> i64
{
    let actual_elapsed_milliseconds = simulation_elapsed_ms(simulation_start_time);
    actual_elapsed_milliseconds as i64 - expected_elapsed_milliseconds as i64
}
 
// Helper: push a sensor reading into the buffer and record common metrics
fn push_reading_and_record_metrics(
    priority_buffer: &Shared<PriorityBuffer>,
    metrics: &Shared<SystemMetrics>,
    sensor_reading: SensorReading,
    drift_milliseconds: i64,
) -> (bool, u64)
{
    // ── Lab 3: Measure buffer insert latency ──────────────────────────────
    let insert_start_time = Instant::now();
    let accepted = priority_buffer.lock().unwrap().push(sensor_reading);
    let insert_latency_microseconds = insert_start_time.elapsed().as_micros() as u64;
 
    // ── Update shared metrics ─────────────────────────────────────────────
    {
        let mut metrics_guard = metrics.lock().unwrap();
        metrics_guard.total_received_readings += 1;
        metrics_guard.insert_latency_microseconds.push(insert_latency_microseconds);
        metrics_guard.drift_milliseconds.push(drift_milliseconds);
 
        if !accepted
        {
            metrics_guard.total_dropped_readings += 1;
        }
    }
 
    (accepted, insert_latency_microseconds)
}
 
// Helper: write a detailed runtime log line to the log file only
fn write_runtime_log(runtime_logger: &Shared<File>, line: &str)
{
    let mut log_file = runtime_logger.lock().unwrap();
    let _ = writeln!(log_file, "{line}");
}
 
// Helper: print important events to terminal AND write them to the runtime log
fn print_and_log_important(runtime_logger: &Shared<File>, line: &str)
{
    println!("{line}");
    write_runtime_log(runtime_logger, line);
}
 
// Pretty-print a single row in the metrics report table
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
 
 
// =============================================================================
//  LOG FILE INITIALISER
//
//  Creates/truncates the runtime log file at startup.
//  Detailed runtime events are written here to keep the terminal readable.
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
//  UDP SENDER THREAD  (Lab 9: UdpSocket.send_to + Lab 11: mpsc consumer)
// =============================================================================
 
fn udp_sender_thread(
    udp_receiver: mpsc::Receiver<String>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
)
{
    // Bind to any available local port — we only need to SEND, not receive
    let socket = UdpSocket::bind("0.0.0.0:0").expect("[UDP-SEND] bind failed");
 
    println!(
        "[OCS] Telemetry link established → {}",
        GCS_TELEMETRY_ADDRESS
    );
    write_runtime_log(
        &runtime_logger,
        &format!(
            "[{}ms] [UDP-SEND] Ready → {}",
            simulation_elapsed_ms(&simulation_start_time),
            GCS_TELEMETRY_ADDRESS
        ),
    );
 
    // Lab 11: "for msg in rx" — the loop ends when all senders are dropped
    for message in &udp_receiver
    {
        socket.send_to(message.as_bytes(), GCS_TELEMETRY_ADDRESS)
            .unwrap_or_else(|error|
            {
                let line = format!(
                    "[OCS] [UDP-SEND] Connection error: {}",
                    error
                );
                print_and_log_important(&runtime_logger, &line);
                0
            });
    }
 
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [UDP-SEND] All senders dropped — exit.",
            simulation_elapsed_ms(&simulation_start_time)
        ),
    );
}
 
 
// =============================================================================
//  COMMAND RECEIVER THREAD  (Lab 9: UdpSocket.recv_from + serde_json)
// =============================================================================
 
fn command_receiver_thread(
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
)
{
    let socket = UdpSocket::bind(OCS_COMMAND_BIND_ADDRESS).expect("[OCS-CMD] bind failed");
 
    // set_read_timeout prevents recv_from from blocking forever,
    // allowing us to check the `running` flag each iteration.
    socket.set_read_timeout(Some(Duration::from_millis(UDP_READ_TIMEOUT))).unwrap();
 
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [OCS-CMD] Listening on {}",
            simulation_elapsed_ms(&simulation_start_time),
            OCS_COMMAND_BIND_ADDRESS
        ),
    );
 
    let mut receive_buffer = [0u8; 4096];
 
    while is_running(&running)
    {
        match socket.recv_from(&mut receive_buffer)
        {
            Ok((received_length, source_address)) =>
            {
                let raw_payload = String::from_utf8_lossy(&receive_buffer[..received_length]);
 
                // serde_json: deserialise the raw bytes into a typed GcsCommand struct
                match serde_json::from_str::<GcsCommand>(&raw_payload)
                {
                    Ok(command) =>
                    {
                        let line = format!(
                            "[{}ms] [OCS-CMD] {} → cmd=\"{}\"  ts={}",
                            simulation_elapsed_ms(&simulation_start_time),
                            source_address,
                            command.cmd,
                            command.ts
                        );
                        print_and_log_important(&runtime_logger, &line);
                    }
                    Err(_) =>
                    {
                        let line = format!(
                            "[{}ms] [OCS-CMD] {} (unparsed): {}",
                            simulation_elapsed_ms(&simulation_start_time),
                            source_address,
                            raw_payload
                        );
                        print_and_log_important(&runtime_logger, &line);
                    }
                }
            }
 
            // WouldBlock / TimedOut are expected — just loop again
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                || error.kind() == std::io::ErrorKind::TimedOut => {}
 
            Err(error) =>
            {
                let line = format!(
                    "[{}ms] [OCS-CMD] {}",
                    simulation_elapsed_ms(&simulation_start_time),
                    error
                );
                print_and_log_important(&runtime_logger, &line);
            }
        }
    }
 
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [OCS-CMD] Exit.",
            simulation_elapsed_ms(&simulation_start_time)
        ),
    );
}
 
 
// =============================================================================
//  PART 1 — SENSOR THREADS  (dedicated OS threads — Lab 2)
// =============================================================================
 
fn thermal_sensor_thread(
    priority_buffer: Shared<PriorityBuffer>,
    metrics: Shared<SystemMetrics>,
    state: Shared<SystemState>,
    emergency: Shared<bool>,
    udp_sender: mpsc::Sender<String>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
)
{
    write_runtime_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Thermal] period={}ms  buf_priority=1  SAFETY-CRITICAL",
            simulation_elapsed_ms(&simulation_start_time),
            THERMAL_SENSOR_PERIOD
        ),
    );
 
    let mut sequence_number: u64 = 0;
 
    while is_running(&running)
    {
        thread::sleep(Duration::from_millis(THERMAL_SENSOR_PERIOD));
 
        let expected_elapsed_milliseconds = (sequence_number + 1) * THERMAL_SENSOR_PERIOD;
        let drift_milliseconds = calculate_drift_milliseconds(
            &simulation_start_time,
            expected_elapsed_milliseconds,
        );
 
        // Simulate a slowly rising temperature
        let temperature_celsius: f64 = 22.0 + (sequence_number % 50) as f64 * 0.3;
 
        // Build the sensor reading with priority 1 (highest / most critical)
        let sensor_reading = SensorReading
        {
            sensor_type: SensorType::Thermal,
            value: temperature_celsius,
            timestamp_ms: simulation_elapsed_ms(&simulation_start_time),
            priority: 1,
        };
 
        let (accepted, _insert_latency_microseconds) = push_reading_and_record_metrics(
            &priority_buffer,
            &metrics,
            sensor_reading,
            drift_milliseconds,
        );
 
        {
            let mut metrics_guard = metrics.lock().unwrap();
 
            // Record jitter (skip seq=0 as there is no previous reference)
            if sequence_number > 0
            {
                // Only jitter uses .as_micros() per your preference:
                // derive expected vs actual timing with Duration, then convert ONLY here.
                let jitter_microseconds = Duration::from_millis(drift_milliseconds.unsigned_abs())
                    .as_micros() as i64;
 
                metrics_guard.thermal_jitter_microseconds.push(jitter_microseconds);
 
                if jitter_microseconds > JITTER_WARNING_LIMIT_MICROSECONDS
                {
                    let violation = format!(
                        "[{}ms] [WARN] Thermal jitter {}µs  seq={}",
                        simulation_elapsed_ms(&simulation_start_time),
                        jitter_microseconds,
                        sequence_number
                    );
 
                    write_runtime_log(&runtime_logger, &violation);
                    metrics_guard.deadline_violations.push(violation);
                }
            }
 
            // ── Lab 7: Consecutive miss counter ────────────────────────────────
            if accepted
            {
                metrics_guard.consecutive_thermal_misses = 0;
            }
            else
            {
                metrics_guard.consecutive_thermal_misses += 1;
 
                let fill_ratio = priority_buffer_fill_ratio(&priority_buffer);
                let drop_message = format!(
                    "[{}ms] [DROP] Thermal seq={}  fill={:.1}%",
                    simulation_elapsed_ms(&simulation_start_time),
                    sequence_number,
                    fill_ratio * 100.0
                );
 
                write_runtime_log(&runtime_logger, &drop_message);
                metrics_guard.dropped_log.push(drop_message);
 
                // Safety alert after MAX_CONSECUTIVE_THERMAL_MISSES consecutive drops
                if metrics_guard.consecutive_thermal_misses >= MAX_CONSECUTIVE_THERMAL_MISSES
                {
                    *emergency.lock().unwrap() = true;
 
                    let alert_message = format!(
                        "[{}ms] !!! SAFETY ALERT !!! {} thermal misses",
                        simulation_elapsed_ms(&simulation_start_time),
                        metrics_guard.consecutive_thermal_misses
                    );
 
                    print_and_log_important(&runtime_logger, &alert_message);
                    metrics_guard.safety_alerts.push(alert_message);
 
                    // serde: send a typed alert to GCS
                    send_ocs_message(
                        &udp_sender,
                        OcsMessage::Alert
                        {
                            event: "thermal_alert".into(),
                            info: AlertInfo
                            {
                                misses: Some(metrics_guard.consecutive_thermal_misses),
                                ..Default::default()
                            },
                        }
                    );
                }
            }
        }
 
        // Transition to Degraded if buffer is >= 80% full
        if priority_buffer_fill_ratio(&priority_buffer) >= DEGRADED_MODE_THRESHOLD
        {
            let mut system_state = state.lock().unwrap();
 
            if *system_state == SystemState::Normal
            {
                let line = format!(
                    "[{}ms] [Thermal] Buffer >= 80% → DEGRADED",
                    simulation_elapsed_ms(&simulation_start_time)
                );
 
                print_and_log_important(&runtime_logger, &line);
                *system_state = SystemState::Degraded;
            }
        }
 
        // Send telemetry to GCS every 5 readings to avoid flooding
        if sequence_number % 5 == 0
        {
            send_ocs_message(
                &udp_sender,
                OcsMessage::Thermal
                {
                    seq: sequence_number,
                    temp: temperature_celsius,
                    drift_ms: drift_milliseconds,
                }
            );
        }
 
        sequence_number += 1;
    }
 
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [Thermal] Exit.",
            simulation_elapsed_ms(&simulation_start_time)
        ),
    );
}
 
 
fn accelerometer_thread(
    priority_buffer: Shared<PriorityBuffer>,
    metrics: Shared<SystemMetrics>,
    udp_sender: mpsc::Sender<String>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
)
{
    write_runtime_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Accel] period={}ms  buf_priority=2",
            simulation_elapsed_ms(&simulation_start_time),
            ACCELEROMETER_PERIOD
        ),
    );
 
    let mut sequence_number: u64 = 0;
 
    while is_running(&running)
    {
        thread::sleep(Duration::from_millis(ACCELEROMETER_PERIOD));
 
        let expected_elapsed_milliseconds = (sequence_number + 1) * ACCELEROMETER_PERIOD;
        let drift_milliseconds = calculate_drift_milliseconds(
            &simulation_start_time,
            expected_elapsed_milliseconds,
        );
 
        // Simulate a 3-axis accelerometer reading
        let acceleration_x = (sequence_number as f64 * 0.05).sin() * 0.10;
        let acceleration_y = (sequence_number as f64 * 0.07).cos() * 0.10;
        let acceleration_z = 9.81 + (sequence_number as f64 * 0.03).sin() * 0.01;
        let magnitude = (acceleration_x * acceleration_x
            + acceleration_y * acceleration_y
            + acceleration_z * acceleration_z).sqrt();
 
        let sensor_reading = SensorReading
        {
            sensor_type: SensorType::Accelerometer,
            value: magnitude,
            timestamp_ms: simulation_elapsed_ms(&simulation_start_time),
            priority: 2,
        };
 
        let (accepted, _insert_latency_microseconds) = push_reading_and_record_metrics(
            &priority_buffer,
            &metrics,
            sensor_reading,
            drift_milliseconds,
        );
 
        {
            let mut metrics_guard = metrics.lock().unwrap();
 
            if sequence_number > 0
            {
                let jitter_microseconds = Duration::from_millis(drift_milliseconds.unsigned_abs())
                    .as_micros() as i64;
 
                metrics_guard.accelerometer_jitter_microseconds.push(jitter_microseconds);
            }
 
            if !accepted
            {
                let drop_message = format!(
                    "[{}ms] [DROP] Accel seq={}",
                    simulation_elapsed_ms(&simulation_start_time),
                    sequence_number
                );
 
                write_runtime_log(&runtime_logger, &drop_message);
                metrics_guard.dropped_log.push(drop_message);
            }
        }
 
        // Send telemetry every 10 readings
        if sequence_number % 10 == 0
        {
            send_ocs_message(
                &udp_sender,
                OcsMessage::Accel
                {
                    seq: sequence_number,
                    mag: magnitude,
                }
            );
        }
 
        sequence_number += 1;
    }
 
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [Accel] Exit.",
            simulation_elapsed_ms(&simulation_start_time)
        ),
    );
}
 
 
// =============================================================================
//  LAB 8 PART 1 — FRAGILE GYROSCOPE
// =============================================================================
 
fn fragile_gyroscope(
    generation: u32,
    priority_buffer: Shared<PriorityBuffer>,
    metrics: Shared<SystemMetrics>,
    udp_sender: mpsc::Sender<String>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
)
{
    write_runtime_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Gyro-{}] period={}ms  buf_priority=3",
            simulation_elapsed_ms(&simulation_start_time),
            generation,
            GYROSCOPE_PERIOD
        ),
    );
 
    let mut sequence_number: u64 = 0;
 
    while is_running(&running)
    {
        thread::sleep(Duration::from_millis(GYROSCOPE_PERIOD));
 
        // ── Lab 8: 3% random panic ────────────────────────────────────────────
        if rand::random_range(0u32..100) < 3
        {
            let line = format!(
                "[{}ms] [Gyro-{}] Hardware fault — panicking!",
                simulation_elapsed_ms(&simulation_start_time),
                generation
            );
 
            print_and_log_important(&runtime_logger, &line);
            panic!("Gyroscope fault (generation {generation})");
        }
 
        // ── Normal operation ──────────────────────────────────────────────────
        let expected_elapsed_milliseconds = (sequence_number + 1) * GYROSCOPE_PERIOD;
        let drift_milliseconds = calculate_drift_milliseconds(
            &simulation_start_time,
            expected_elapsed_milliseconds,
        );
 
        let angular_velocity_z: f64 = 0.5 * (sequence_number as f64 * 0.10).sin();
 
        let sensor_reading = SensorReading
        {
            sensor_type: SensorType::Gyroscope,
            value: angular_velocity_z,
            timestamp_ms: simulation_elapsed_ms(&simulation_start_time),
            priority: 3,
        };
 
        let (accepted, _insert_latency_microseconds) = push_reading_and_record_metrics(
            &priority_buffer,
            &metrics,
            sensor_reading,
            drift_milliseconds,
        );
 
        {
            let mut metrics_guard = metrics.lock().unwrap();
 
            if sequence_number > 0
            {
                let jitter_microseconds = Duration::from_millis(drift_milliseconds.unsigned_abs())
                    .as_micros() as i64;
 
                metrics_guard.gyroscope_jitter_microseconds.push(jitter_microseconds);
            }
 
            if !accepted
            {
                let drop_message = format!(
                    "[{}ms] [DROP] Gyro seq={}",
                    simulation_elapsed_ms(&simulation_start_time),
                    sequence_number
                );
 
                write_runtime_log(&runtime_logger, &drop_message);
                metrics_guard.dropped_log.push(drop_message);
            }
        }
 
        if sequence_number % 25 == 0
        {
            send_ocs_message(
                &udp_sender,
                OcsMessage::Gyro
                {
                    generation,
                    seq: sequence_number,
                    omega_z: angular_velocity_z,
                }
            );
        }
 
        sequence_number += 1;
    }
}
 
 
// =============================================================================
//  LAB 8 PART 2 — GYROSCOPE SUPERVISOR
// =============================================================================
 
fn gyroscope_supervisor_thread(
    priority_buffer: Shared<PriorityBuffer>,
    metrics: Shared<SystemMetrics>,
    udp_sender: mpsc::Sender<String>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
)
{
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [Gyro-SUP] Supervisor online.",
            simulation_elapsed_ms(&simulation_start_time)
        ),
    );
 
    let mut generation: u32 = 0;
 
    while is_running(&running)
    {
        generation += 1;
 
        print_and_log_important(
            &runtime_logger,
            &format!(
                "[{}ms] [Gyro-SUP] Starting generation {}...",
                simulation_elapsed_ms(&simulation_start_time),
                generation
            ),
        );
 
        // Clone Arcs for the new child thread
        let child_priority_buffer = Arc::clone(&priority_buffer);
        let child_metrics = Arc::clone(&metrics);
        let child_udp_sender = udp_sender.clone();
        let child_running = Arc::clone(&running);
        let child_runtime_logger = Arc::clone(&runtime_logger);
        let child_simulation_start_time = simulation_start_time;
 
        // Lab 8: spawn the fragile worker
        let gyroscope_handle = thread::spawn(move ||
        {
            fragile_gyroscope(
                generation,
                child_priority_buffer,
                child_metrics,
                child_udp_sender,
                child_running,
                child_runtime_logger,
                child_simulation_start_time,
            )
        });
 
        // Lab 8: match handle.join() to detect panic vs normal exit
        match gyroscope_handle.join()
        {
            Ok(_) =>
            {
                // Normal exit — likely because running=false
                print_and_log_important(
                    &runtime_logger,
                    &format!(
                        "[{}ms] [Gyro-SUP] Normal exit — done.",
                        simulation_elapsed_ms(&simulation_start_time)
                    ),
                );
                return;
            }
 
            Err(_) =>
            {
                {
                    let mut metrics_guard = metrics.lock().unwrap();
                    metrics_guard.gyroscope_restart_count += 1;
                }
 
                let restart_line = format!(
                    "[{}ms] [Gyro-SUP] Panic! Restarting in 1s...",
                    simulation_elapsed_ms(&simulation_start_time)
                );
                print_and_log_important(&runtime_logger, &restart_line);
 
                // Notify GCS of the restart via serde-encoded alert
                send_ocs_message(
                    &udp_sender,
                    OcsMessage::Alert
                    {
                        event: "gyro_restart".into(),
                        info: AlertInfo
                        {
                            generation: Some(generation),
                            ..Default::default()
                        },
                    }
                );
 
                // Backoff before restarting
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
 
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [Gyro-SUP] Exit.",
            simulation_elapsed_ms(&simulation_start_time)
        ),
    );
}
 
 
// =============================================================================
//  PART 2 — RM BACKGROUND TASKS via ScheduledThreadPool  (Lab 11)
// =============================================================================
 
// ── Health Monitor (RM P1) ────────────────────────────────────────────────────
// Reports buffer fill, system state, and drift every 200ms.
// Sends OcsMessage::Status telemetry to GCS via UDP.
fn make_health_monitor_task(
    priority_buffer: Shared<PriorityBuffer>,
    metrics: Shared<SystemMetrics>,
    state: Shared<SystemState>,
    udp_sender: mpsc::Sender<String>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
) -> impl FnMut() + Send + 'static
{
    let mut iteration: u64 = 0;
 
    move ||
    {
        if !is_running(&running)
        {
            return;
        }
 
        let active_work_start_time = Instant::now();
 
        // Drift = how late is this task compared to its expected schedule
        let expected_elapsed_milliseconds = iteration * HEALTH_MONITOR_PERIOD;
        let drift_milliseconds = calculate_drift_milliseconds(
            &simulation_start_time,
            expected_elapsed_milliseconds,
        );
 
        let (fill_ratio, buffer_length) =
        {
            let buffer_guard = priority_buffer.lock().unwrap();
            (buffer_guard.fill_ratio(), buffer_guard.heap.len())
        };
 
        let system_state = state.lock().unwrap().clone();
 
        let detail_line = format!(
            "[{}ms] [Health] iter={:>4}  buf={}/{} ({:.1}%)  state={:?}  drift={:+}ms",
            simulation_elapsed_ms(&simulation_start_time),
            iteration,
            buffer_length,
            PRIORITY_BUFFER_CAPACITY,
            fill_ratio * 100.0,
            system_state,
            drift_milliseconds
        );
        write_runtime_log(&runtime_logger, &detail_line);
 
        // Flag if health task itself is drifting (deadline violation)
        if drift_milliseconds.abs() > 5
        {
            let violation = format!(
                "[{}ms] [DEADLINE] Health drift={:+}ms iter={}",
                simulation_elapsed_ms(&simulation_start_time),
                drift_milliseconds,
                iteration
            );
 
            write_runtime_log(&runtime_logger, &violation);
            log_deadline_violation(&metrics, violation);
        }
 
        // serde: send status telemetry to GCS
        send_ocs_message(
            &udp_sender,
            OcsMessage::Status
            {
                iter: iteration,
                fill: fill_ratio as f64 * 100.0,
                state: format!("{system_state:?}"),
                drift_ms: drift_milliseconds,
            }
        );
 
        let active_work_duration_ms = active_work_start_time.elapsed().as_millis() as u64;
 
        {
            let mut metrics_guard = metrics.lock().unwrap();
            metrics_guard.drift_milliseconds.push(drift_milliseconds);
            metrics_guard.active_time_milliseconds += active_work_duration_ms;
            metrics_guard.elapsed_time_milliseconds = simulation_elapsed_ms(&simulation_start_time);
        }
 
        iteration += 1;
    }
}
 
// ── Data Compression Task (RM P2) ─────────────────────────────────────────────
// Drains up to 20 readings from the priority buffer, "compresses" them
// (simulated as 50% size reduction), and enqueues a DataPacket for downlink.
fn make_data_compression_task(
    priority_buffer: Shared<PriorityBuffer>,
    downlink_queue: Shared<Vec<DataPacket>>,
    metrics: Shared<SystemMetrics>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
) -> impl FnMut() + Send + 'static
{
    let mut packet_id: u64 = 0;
    let mut iteration: u64 = 0;
 
    move ||
    {
        if !is_running(&running)
        {
            return;
        }
 
        let active_work_start_time = Instant::now();
 
        let expected_elapsed_milliseconds = iteration * DATA_COMPRESSION_PERIOD;
        let drift_milliseconds = calculate_drift_milliseconds(
            &simulation_start_time,
            expected_elapsed_milliseconds,
        );
 
        // ── Lab 11: drain up to 20 items with .pop() (priority order) ─────────
        let mut sensor_batch: Vec<SensorReading> = Vec::new();
        {
            let mut buffer_guard = priority_buffer.lock().unwrap();
 
            for _ in 0..20
            {
                match buffer_guard.pop()
                {
                    Some(sensor_reading) => sensor_batch.push(sensor_reading),
                    None => break,
                }
            }
        }
 
        if !sensor_batch.is_empty()
        {
            // serde_json::json! macro builds a structured batch payload
            let data_array: Vec<serde_json::Value> = sensor_batch.iter()
                .map(|sensor_reading| serde_json::json!(
                {
                    "sensor": format!("{:?}", sensor_reading.sensor_type),
                    "value": sensor_reading.value,
                    "timestamp_ms": sensor_reading.timestamp_ms,
                }))
                .collect();
 
            let raw_payload = serde_json::json!(
            {
                "packet_id": packet_id,
                "reading_count": sensor_batch.len(),
                "created_at_ms": simulation_elapsed_ms(&simulation_start_time),
                "data": data_array,
            }).to_string();
 
            // Simulate ~50% compression ratio
            let compressed_size_bytes = (raw_payload.len() / 2).max(1);
 
            // Lab 3: measure queue insert latency
            let queue_insert_start_time = Instant::now();
 
            downlink_queue.lock().unwrap().push(DataPacket
            {
                packet_id,
                payload: raw_payload,
                created_at_ms: simulation_elapsed_ms(&simulation_start_time),
                size_bytes: compressed_size_bytes,
            });
 
            let queue_insert_latency_microseconds = queue_insert_start_time.elapsed().as_micros() as u64;
 
            let detail_line = format!(
                "[{}ms] [Compress] packet={}  n={}  {}B  q_insert={}µs  drift={:+}ms",
                simulation_elapsed_ms(&simulation_start_time),
                packet_id,
                sensor_batch.len(),
                compressed_size_bytes,
                queue_insert_latency_microseconds,
                drift_milliseconds
            );
            write_runtime_log(&runtime_logger, &detail_line);
 
            packet_id += 1;
        }
 
        if drift_milliseconds.abs() > 10
        {
            let violation = format!(
                "[{}ms] [DEADLINE] Compress drift={:+}ms iter={}",
                simulation_elapsed_ms(&simulation_start_time),
                drift_milliseconds,
                iteration
            );
 
            write_runtime_log(&runtime_logger, &violation);
            log_deadline_violation(&metrics, violation);
        }
 
        {
            let mut metrics_guard = metrics.lock().unwrap();
            metrics_guard.drift_milliseconds.push(drift_milliseconds);
            metrics_guard.active_time_milliseconds += active_work_start_time.elapsed().as_millis() as u64;
        }
 
        iteration += 1;
    }
}
 
// ── Antenna Alignment Task (RM P3) ────────────────────────────────────────────
// Lowest priority RM task. Can be preempted (skipped) when emergency=true
// or system is in MissionAbort state — demonstrating priority-based preemption.
fn make_antenna_alignment_task(
    metrics: Shared<SystemMetrics>,
    state: Shared<SystemState>,
    emergency: Shared<bool>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
) -> impl FnMut() + Send + 'static
{
    let mut iteration: u64 = 0;
 
    move ||
    {
        if !is_running(&running)
        {
            return;
        }
 
        // ── Preemption check: skip if emergency or abort ───────────────────────
        if *emergency.lock().unwrap() || *state.lock().unwrap() == SystemState::MissionAbort
        {
            let line = format!(
                "[{}ms] [Antenna] iter={:>4}  PREEMPTED",
                simulation_elapsed_ms(&simulation_start_time),
                iteration
            );
 
            print_and_log_important(&runtime_logger, &line);
 
            let violation = format!(
                "[{}ms] [PREEMPT] Antenna iter={}",
                simulation_elapsed_ms(&simulation_start_time),
                iteration
            );
            log_deadline_violation(&metrics, violation);
 
            iteration += 1;
            return;
        }
 
        let active_work_start_time = Instant::now();
 
        let expected_elapsed_milliseconds = iteration * ANTENNA_ALIGNMENT_PERIOD;
        let drift_milliseconds = calculate_drift_milliseconds(
            &simulation_start_time,
            expected_elapsed_milliseconds,
        );
 
        // Compute azimuth and elevation for this iteration
        let azimuth_degrees = (iteration as f64 * 7.2) % 360.0;
        let elevation_degrees = 30.0 + 20.0 * (iteration as f64 * 0.05).sin();
 
        let detail_line = format!(
            "[{}ms] [Antenna] iter={:>4}  az={:>6.1}deg  el={:>5.1}deg  drift={:+}ms",
            simulation_elapsed_ms(&simulation_start_time),
            iteration,
            azimuth_degrees,
            elevation_degrees,
            drift_milliseconds
        );
        write_runtime_log(&runtime_logger, &detail_line);
 
        if drift_milliseconds.abs() > 20
        {
            let violation = format!(
                "[{}ms] [DEADLINE] Antenna drift={:+}ms iter={}",
                simulation_elapsed_ms(&simulation_start_time),
                drift_milliseconds,
                iteration
            );
 
            write_runtime_log(&runtime_logger, &violation);
            log_deadline_violation(&metrics, violation);
        }
 
        {
            let mut metrics_guard = metrics.lock().unwrap();
            metrics_guard.drift_milliseconds.push(drift_milliseconds);
            metrics_guard.active_time_milliseconds += active_work_start_time.elapsed().as_millis() as u64;
        }
 
        iteration += 1;
    }
}
 
 
// =============================================================================
//  PART 3 — DOWNLINK THREAD  (Radio typestate from Lab 7)
// =============================================================================
 
fn downlink_thread(
    downlink_queue: Shared<Vec<DataPacket>>,
    metrics: Shared<SystemMetrics>,
    state: Shared<SystemState>,
    udp_sender: mpsc::Sender<String>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
)
{
    write_runtime_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Downlink] visibility={}s  window={}ms  (Radio typestate active)",
            simulation_elapsed_ms(&simulation_start_time),
            VISIBILITY_WINDOW_INTERVAL,
            DOWNLINK_WINDOW_DURATION
        ),
    );
 
    while is_running(&running)
    {
        // Wait for next visibility window
        thread::sleep(Duration::from_secs(VISIBILITY_WINDOW_INTERVAL));
 
        if !is_running(&running)
        {
            break;
        }
 
        let visibility_line = format!(
            "[{}ms] [Downlink] === Visibility window ===",
            simulation_elapsed_ms(&simulation_start_time)
        );
        write_runtime_log(&runtime_logger, &visibility_line);
 
        // Check if the downlink queue is overflowing → Degraded
        let queue_fill_ratio = downlink_queue.lock().unwrap().len() as f32 / PRIORITY_BUFFER_CAPACITY as f32;
 
        if queue_fill_ratio >= DEGRADED_MODE_THRESHOLD
        {
            let mut system_state = state.lock().unwrap();
 
            if *system_state == SystemState::Normal
            {
                let line = format!(
                    "[{}ms] [Downlink] Queue >= 80% → DEGRADED",
                    simulation_elapsed_ms(&simulation_start_time)
                );
 
                write_runtime_log(&runtime_logger, &line);
                *system_state = SystemState::Degraded;
            }
        }
 
        // ── Lab 7: Typestate transition — Radio<Idle> → Radio<Transmitting> ───
        let radio = Radio::<Idle>::new();
 
        match radio.initialise(&simulation_start_time, &runtime_logger)
        {
            None =>
            {
                // Initialisation failed (exceeded 5ms deadline)
                let violation = format!(
                    "[{}ms] [DEADLINE] Radio initialisation exceeded {}ms",
                    simulation_elapsed_ms(&simulation_start_time),
                    DOWNLINK_INITIALISATION_DEADLINE
                );
 
                write_runtime_log(&runtime_logger, &violation);
                log_deadline_violation(&metrics, violation);
            }
 
            Some(transmitting_radio) =>
            {
                // Drain all queued packets for transmission
                let queued_packets: Vec<DataPacket> = downlink_queue.lock().unwrap().drain(..).collect();
 
                if queued_packets.is_empty()
                {
                    let line = format!(
                        "[{}ms] [Downlink] Nothing queued.",
                        simulation_elapsed_ms(&simulation_start_time)
                    );
 
                    write_runtime_log(&runtime_logger, &line);
                }
 
                // transmit() consumes Radio<Transmitting> and returns Radio<Idle>
                let _idle_radio = transmitting_radio.transmit(
                    &queued_packets,
                    &metrics,
                    &udp_sender,
                    &simulation_start_time,
                    &runtime_logger,
                );
            }
        }
    }
 
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [Downlink] Exit.",
            simulation_elapsed_ms(&simulation_start_time)
        ),
    );
}
 
 
// =============================================================================
//  PART 4 — FAULT INJECTOR  (Lab 7: enum FaultType + Lab 3: recovery timing)
// =============================================================================
 
fn fault_injector_thread(
    metrics: Shared<SystemMetrics>,
    state: Shared<SystemState>,
    emergency: Shared<bool>,
    udp_sender: mpsc::Sender<String>,
    running: Shared<bool>,
    runtime_logger: Shared<File>,
    simulation_start_time: Instant,
)
{
    write_runtime_log(
        &runtime_logger,
        &format!(
            "[{}ms] [Faults] interval={}s  recovery_limit={}ms",
            simulation_elapsed_ms(&simulation_start_time),
            FAULT_INJECTION_INTERVAL,
            RECOVERY_TIME_LIMIT
        ),
    );
 
    let mut fault_count: u32 = 0;
 
    while is_running(&running)
    {
        thread::sleep(Duration::from_secs(FAULT_INJECTION_INTERVAL));
 
        if !is_running(&running)
        {
            break;
        }
 
        fault_count += 1;
 
        // ── Lab 7: random fault selection ─────────────────────────────────────
        let injected_fault = match rand::random_range(0u32..3)
        {
            0 => FaultType::SensorBusHang(80),
            1 => FaultType::CorruptedReading,
            _ => FaultType::PowerSpike,
        };
 
        let fault_message = format!(
            "[{}ms] [FAULT #{}] {:?}",
            simulation_elapsed_ms(&simulation_start_time),
            fault_count,
            injected_fault
        );
 
        print_and_log_important(&runtime_logger, &fault_message);
 
        {
            let mut metrics_guard = metrics.lock().unwrap();
            metrics_guard.fault_log.push(fault_message);
        }
 
        // serde: alert the GCS of the fault
        send_ocs_message(
            &udp_sender,
            OcsMessage::Alert
            {
                event: "fault".into(),
                info: AlertInfo
                {
                    count: Some(fault_count),
                    fault_type: Some(format!("{:?}", injected_fault)),
                    ..Default::default()
                },
            }
        );
 
        // ── Lab 7: match on fault variant to decide recovery action ───────────
        match injected_fault
        {
            FaultType::SensorBusHang(hang_duration_milliseconds) =>
            {
                let line = format!(
                    "[{}ms] [Faults] {}ms bus hang...",
                    simulation_elapsed_ms(&simulation_start_time),
                    hang_duration_milliseconds
                );
                print_and_log_important(&runtime_logger, &line);
 
                thread::sleep(Duration::from_millis(hang_duration_milliseconds));
            }
 
            FaultType::CorruptedReading =>
            {
                let line = format!(
                    "[{}ms] [Faults] Corrupted reading injected",
                    simulation_elapsed_ms(&simulation_start_time)
                );
                print_and_log_important(&runtime_logger, &line);
            }
 
            FaultType::PowerSpike =>
            {
                let line = format!(
                    "[{}ms] [Faults] Power spike → DEGRADED + emergency",
                    simulation_elapsed_ms(&simulation_start_time)
                );
                print_and_log_important(&runtime_logger, &line);
 
                *state.lock().unwrap() = SystemState::Degraded;
                *emergency.lock().unwrap() = true;
            }
        }
 
        // ── Lab 3: Measure recovery time ─────────────────────────────────────
        let recovery_start_time = Instant::now();
 
        let recovery_line = format!(
            "[{}ms] [Faults] Recovering...",
            simulation_elapsed_ms(&simulation_start_time)
        );
        print_and_log_important(&runtime_logger, &recovery_line);
 
        thread::sleep(Duration::from_millis(50));  // simulate recovery work
 
        // Clear fault state
        *emergency.lock().unwrap() = false;
        *state.lock().unwrap() = SystemState::Normal;
 
        let recovery_time_milliseconds = recovery_start_time.elapsed().as_millis() as u64;
 
        let recovery_done_line = format!(
            "[{}ms] [Faults] Recovery in {}ms",
            simulation_elapsed_ms(&simulation_start_time),
            recovery_time_milliseconds
        );
        print_and_log_important(&runtime_logger, &recovery_done_line);
 
        {
            let mut metrics_guard = metrics.lock().unwrap();
            metrics_guard.recovery_times_milliseconds.push(recovery_time_milliseconds);
 
            if recovery_time_milliseconds > RECOVERY_TIME_LIMIT
            {
                let abort_message = format!(
                    "[{}ms] !!! MISSION ABORT !!! recovery {}ms",
                    simulation_elapsed_ms(&simulation_start_time),
                    recovery_time_milliseconds
                );
 
                print_and_log_important(&runtime_logger, &abort_message);
                metrics_guard.safety_alerts.push(abort_message);
                *state.lock().unwrap() = SystemState::MissionAbort;
            }
        }
    }
 
    print_and_log_important(
        &runtime_logger,
        &format!(
            "[{}ms] [Faults] Exit.",
            simulation_elapsed_ms(&simulation_start_time)
        ),
    );
}
 
 
// =============================================================================
//  FINAL METRICS REPORTER
//
//  Per your preference, reporting is done ONLY at the end of the simulation.
//  No periodic reporter thread is used anymore.
//
//  This keeps the terminal clean and matches the assignment requirement of
//  collecting runtime logs continuously while presenting an evaluation summary
//  after execution.
// =============================================================================
 
fn print_final_report(metrics: &SystemMetrics, simulation_end_time_ms: u64)
{
    let packet_loss_percentage = if metrics.total_received_readings > 0
    {
        metrics.total_dropped_readings as f64 / metrics.total_received_readings as f64 * 100.0
    }
    else
    {
        0.0
    };
 
    let approximate_cpu_utilisation = if metrics.elapsed_time_milliseconds > 0
    {
        metrics.active_time_milliseconds as f64 / metrics.elapsed_time_milliseconds as f64 * 100.0
    }
    else
    {
        0.0
    };
 
    println!("\n╔══════════════════════════════════════════════════════════════════════╗");
    println!("║  OCS FINAL REPORT  at {}ms", simulation_end_time_ms);
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!(
        "║  Rx: {}  Dropped: {} ({:.2}%)  Gyro restarts: {}",
        metrics.total_received_readings,
        metrics.total_dropped_readings,
        packet_loss_percentage,
        metrics.gyroscope_restart_count
    );
    println!("║  Jitter (µs)  limit={}µs", JITTER_WARNING_LIMIT_MICROSECONDS);
    print_stat_row("Thermal [CRITICAL]", &metrics.thermal_jitter_microseconds);
    print_stat_row("Accelerometer", &metrics.accelerometer_jitter_microseconds);
    print_stat_row("Gyroscope", &metrics.gyroscope_jitter_microseconds);
 
    println!("║  Drift (ms)");
    print_stat_row("All tasks", &metrics.drift_milliseconds);
 
    println!("║  Insert latency (µs)");
    let insert_latency_values: Vec<i64> = metrics.insert_latency_microseconds
        .iter()
        .map(|&value| value as i64)
        .collect();
    print_stat_row("priority_buffer.push()", &insert_latency_values);
 
    println!("║  Deadline violations: {}", metrics.deadline_violations.len());
    for violation in metrics.deadline_violations.iter().take(5)
    {
        println!("║    {violation}");
    }
    if metrics.deadline_violations.len() > 5
    {
        println!(
            "║    ... and {} more",
            metrics.deadline_violations.len() - 5
        );
    }
 
    println!("║  Faults: {}", metrics.fault_log.len());
    for fault in &metrics.fault_log
    {
        println!("║    {fault}");
    }
 
    if !metrics.recovery_times_milliseconds.is_empty()
    {
        let maximum_recovery_time = metrics.recovery_times_milliseconds.iter().max().unwrap();
        let average_recovery_time = metrics.recovery_times_milliseconds.iter().sum::<u64>() as f64
            / metrics.recovery_times_milliseconds.len() as f64;
 
        println!(
            "║    recovery max={}ms  avg={:.1}ms",
            maximum_recovery_time,
            average_recovery_time
        );
    }
 
    println!("║  CPU ≈ {:.2}%", approximate_cpu_utilisation);
 
    if !metrics.safety_alerts.is_empty()
    {
        println!("║  SAFETY ALERTS:");
        for alert in &metrics.safety_alerts
        {
            println!("║    {alert}");
        }
    }
 
    println!("╚══════════════════════════════════════════════════════════════════════╝\n");
}

// =============================================================================
// MAIN FUNCTION
//
// Sets up all shared state, spawns all OS threads, schedules RM background
// tasks via ScheduledThreadPool, runs for the simulation duration, then
// shuts everything down gracefully and prints a final end-of-run report.
// =============================================================================

fn main()
{
    println!("╔═══════════════════════════════════════════════════════╗");
    println!("║  OCS — Satellite Onboard Control System              ║");
    println!("║  CT087-3-3  |  Student A  |  Hard RTS / OS threads   ║");
    println!("╚═══════════════════════════════════════════════════════╝\n");

    // ── Simulation Clock / Logger Initialization ─────────────────────────────
    let simulation_start_time: Instant = Instant::now();
    let runtime_logger: Shared<File> = create_runtime_logger();

    write_runtime_log(&runtime_logger, "OCS startup sequence initiated.");

    // ── Shared State Setup ────────────────────────────────────────────────────
    let shared_priority_buffer: Shared<PriorityBuffer> =
        Arc::new(Mutex::new(PriorityBuffer::new(PRIORITY_BUFFER_CAPACITY)));

    let shared_system_metrics: Shared<SystemMetrics> =
        Arc::new(Mutex::new(SystemMetrics::new()));

    let shared_system_state: Shared<SystemState> =
        Arc::new(Mutex::new(SystemState::Normal));

    let shared_downlink_data_queue: Shared<Vec<DataPacket>> =
        Arc::new(Mutex::new(Vec::<DataPacket>::with_capacity(50)));

    let shared_emergency_flag: Shared<bool> =
        Arc::new(Mutex::new(false));

    let shared_running_flag: Shared<bool> =
        Arc::new(Mutex::new(true));

    // ── UDP Telemetry Channel ────────────────────────────────────────────────
    let (telemetry_sender, telemetry_receiver) = mpsc::channel::<String>();

    println!("[OCS] Configuration:");
    println!("      Telemetry → GCS : {GCS_TELEMETRY_ADDRESS}  (serde encoded)");
    println!("      Commands  ← GCS : {OCS_COMMAND_BIND_ADDRESS}  (serde decoded)");
    println!("      Buffer capacity : {PRIORITY_BUFFER_CAPACITY}");
    println!("      Radio typestate : Idle → Transmitting");
    println!("      RM pool tasks   : health / compress / antenna");
    println!("      Sim duration    : {SIMULATION_DURATION_SECONDS}s\n");

    write_runtime_log(&runtime_logger, "System configuration completed.");

    // ── Thread Handles ───────────────────────────────────────────────────────
    let mut operating_system_thread_handles: Vec<thread::JoinHandle<()>> = Vec::new();

    // UDP sender thread
    operating_system_thread_handles.push(thread::spawn({
        let logger = Arc::clone(&runtime_logger);
        let start  = simulation_start_time;
        let rx     = telemetry_receiver;
        move || udp_sender_thread(rx, logger, start)
    }));

    // Command receiver thread
    operating_system_thread_handles.push(thread::spawn({
        let running = Arc::clone(&shared_running_flag);
        let logger  = Arc::clone(&runtime_logger);
        let start   = simulation_start_time;
        move || command_receiver_thread(running, logger, start)
    }));

    // Thermal sensor thread
    operating_system_thread_handles.push(thread::spawn({
        let buf       = Arc::clone(&shared_priority_buffer);
        let metrics   = Arc::clone(&shared_system_metrics);
        let state     = Arc::clone(&shared_system_state);
        let emergency = Arc::clone(&shared_emergency_flag);
        let tx        = telemetry_sender.clone();
        let running   = Arc::clone(&shared_running_flag);
        let logger    = Arc::clone(&runtime_logger);
        let start     = simulation_start_time;
        move || thermal_sensor_thread(buf, metrics, state, emergency, tx, running, logger, start)
    }));

    // Accelerometer thread
    operating_system_thread_handles.push(thread::spawn({
        let buf     = Arc::clone(&shared_priority_buffer);
        let metrics = Arc::clone(&shared_system_metrics);
        let tx      = telemetry_sender.clone();
        let running = Arc::clone(&shared_running_flag);
        let logger  = Arc::clone(&runtime_logger);
        let start   = simulation_start_time;
        move || accelerometer_thread(buf, metrics, tx, running, logger, start)
    }));

    // Gyroscope supervisor thread
    operating_system_thread_handles.push(thread::spawn({
        let buf     = Arc::clone(&shared_priority_buffer);
        let metrics = Arc::clone(&shared_system_metrics);
        let tx      = telemetry_sender.clone();
        let running = Arc::clone(&shared_running_flag);
        let logger  = Arc::clone(&runtime_logger);
        let start   = simulation_start_time;
        move || gyroscope_supervisor_thread(buf, metrics, tx, running, logger, start)
    }));

    // ── ScheduledThreadPool for RM Background Tasks ──────────────────────────
    let rate_monotonic_thread_pool = ScheduledThreadPool::new(3);

    rate_monotonic_thread_pool.execute_at_fixed_rate(
        Duration::from_millis(10),
        Duration::from_millis(HEALTH_MONITOR_PERIOD),
        make_health_monitor_task(
            Arc::clone(&shared_priority_buffer),
            Arc::clone(&shared_system_metrics),
            Arc::clone(&shared_system_state),
            telemetry_sender.clone(),
            Arc::clone(&shared_running_flag),
            Arc::clone(&runtime_logger),
            simulation_start_time,
        ),
    );

    rate_monotonic_thread_pool.execute_at_fixed_rate(
        Duration::from_millis(20),
        Duration::from_millis(DATA_COMPRESSION_PERIOD),
        make_data_compression_task(
            Arc::clone(&shared_priority_buffer),
            Arc::clone(&shared_downlink_data_queue),
            Arc::clone(&shared_system_metrics),
            Arc::clone(&shared_running_flag),
            Arc::clone(&runtime_logger),
            simulation_start_time,
        ),
    );

    rate_monotonic_thread_pool.execute_at_fixed_rate(
        Duration::from_millis(30),
        Duration::from_millis(ANTENNA_ALIGNMENT_PERIOD),
        make_antenna_alignment_task(
            Arc::clone(&shared_system_metrics),
            Arc::clone(&shared_system_state),
            Arc::clone(&shared_emergency_flag),
            Arc::clone(&shared_running_flag),
            Arc::clone(&runtime_logger),
            simulation_start_time,
        ),
    );

    // Downlink thread
    operating_system_thread_handles.push(thread::spawn({
        let dq      = Arc::clone(&shared_downlink_data_queue);
        let metrics = Arc::clone(&shared_system_metrics);
        let state   = Arc::clone(&shared_system_state);
        let tx      = telemetry_sender.clone();
        let running = Arc::clone(&shared_running_flag);
        let logger  = Arc::clone(&runtime_logger);
        let start   = simulation_start_time;
        move || downlink_thread(dq, metrics, state, tx, running, logger, start)
    }));

    // Fault injector thread
    operating_system_thread_handles.push(thread::spawn({
        let metrics   = Arc::clone(&shared_system_metrics);
        let state     = Arc::clone(&shared_system_state);
        let emergency = Arc::clone(&shared_emergency_flag);
        let tx        = telemetry_sender.clone();
        let running   = Arc::clone(&shared_running_flag);
        let logger    = Arc::clone(&runtime_logger);
        let start     = simulation_start_time;
        move || fault_injector_thread(metrics, state, emergency, tx, running, logger, start)
    }));

    // Drop the final sender so udp_sender_thread can exit cleanly
    drop(telemetry_sender);

    println!(
        "[OCS] {} OS threads + 3 ScheduledThreadPool tasks online. Running for {}s...\n",
        operating_system_thread_handles.len(),
        SIMULATION_DURATION_SECONDS
    );

    write_runtime_log(&runtime_logger, "All OCS threads and RM tasks are online.");

    // ── Run for the Simulation Duration ──────────────────────────────────────
    thread::sleep(Duration::from_secs(SIMULATION_DURATION_SECONDS));

    // ── Graceful Shutdown ─────────────────────────────────────────────────────
    println!("\n[OCS] Simulation ended — signalling shutdown...");
    write_runtime_log(&runtime_logger, "Simulation duration elapsed. Shutdown initiated.");

    *shared_running_flag.lock().unwrap() = false;

    drop(rate_monotonic_thread_pool);

    for handle in operating_system_thread_handles
    {
        let _ = handle.join();
    }

    write_runtime_log(&runtime_logger, "All OCS threads joined successfully.");

    // ── Final Summary Report ──────────────────────────────────────────────────
    println!("\n[OCS] FINAL REPORT:");
    {
        let metrics_guard = shared_system_metrics.lock().unwrap();
        let end_ms = simulation_elapsed_ms(&simulation_start_time);
        print_final_report(&metrics_guard, end_ms);
    }

    write_runtime_log(&runtime_logger, "Final report generated.");
    println!("[OCS] Done.");
}