// SATELLITE ONBOARD CONTROL SYSTEM - BY LUVEN MARK (TP071542)
// TYPE: HARD RTOS, demonstrating the learnt Hard RTS concepts

// Standard Library Imports
use std::collections::BinaryHeap;   // for priority queue
use std::fs::{File, OpenOptions};   // for runtime log file
use std::io::Write;                 // for writing log lines
use std::marker::PhantomData;       // for typestate
use std::net::UdpSocket;            // for UDP
use std::sync::{Arc, Mutex};        // for shared ownership across threads 
use std::sync::mpsc;                // for message passing between threads
use std::thread;                    // for OS thread creation (Hard RTS)
use std::time::{Duration, Instant}; // for timing

// Other Imports
use scheduled_thread_pool::ScheduledThreadPool; // thread pool for Rate Monotonic Scheduling tasks
use serde::{Deserialize, Serialize};            // automatic JSON serialisation/deserialisation
use serde_json;                                 // JSON encode/decode for UDP payloads


// ~~~~ SECTION 0: PRE-COMPILATION ~~~~~
// ---- 1. CONSTANTS -----
// Uses all caps with snake case due to warning

// Sensor Sampling Periods (ms)
const THERMAL_PERIOD: u64 = 100;         // highest priority sensor, fastest rate
const ACCELEROMETER_PERIOD: u64 = 200;
const GYROSCOPE_PERIOD: u64 = 300;      // Period (slides definition) = time between 2 tasks (interval)
 
// Background Task Periods (ms)
const HEALTH_MONITOR_PERIOD: u64 = 100;
const DATA_COMPRESSION_PERIOD: u64 = 200;   
const ANTENNA_ALIGNMENT_PERIOD: u64 = 500;
 
// Safety Thresholds (Thermal = Critical Sensor)
const THERMAL_JITTER_TIME_LIMIT: i64 = 1000;      // (μs) precise jitter measurement for critical sensor, max 1ms
const THERMAL_MAX_MISSES: u32 = 3;                // for safety alert after 3 consecutive misses
const PRIORITY_BUFFER: usize = 100;               // max items in the priority buffer
const DEGRADED_MODE: f32 = 0.80;                  // degraded mode at 80% full
 
// Downlink (ms)
const VISIBILITY_PERIOD: u64 = 5000;        // 5000ms = 5s
const DOWNLINK_TIME_LIMIT: u64 = 30;     
const INITIALISE_TIME_LIMIT: u64 = 5;      
 
// Fault Injection & Simulation
const FAULT_INJECTION_PERIOD: u64 = 60;   // 60s = 60000ms
const RECOVERY_TIME_LIMIT: u64 = 200000;     // 200000μs = 200ms    
const SIMULATION_DURATION: u64 = 125;     // 125s = 125000ms = 2min 5sec to inject 3 faults
 
// Pre-allocated worst case Vec capacities during compilation to avoiding unpredictable heap allocation latency
const JITTER_MAX_SIZE: usize = 10000;
const DRIFT_MAX_SIZE: usize = 20000;
const LATENCY_MAX_SIZE: usize = 20000;
const LOG_MAX_SIZE: usize = 500;
 
// UDP Communication
const GCS_TELEMETRY_ADDRESS: &str = "127.0.0.1:9000";
const OCS_COMMAND_ADDRESS: &str = "0.0.0.0:9001";
const UDP_TIMEOUT: u64 = 100;
 
// Simulation Log
const LOG_FILE: &str = "ocs.log";
 

// ---- 2. DATA STRUCTURES ----

// Main Message Enum to be serialised and sent to GCS
#[derive(Serialize, Deserialize, Debug)]
enum OCSMessage
{
    Status
    {
        iteration: u64,
        fill: f64, 
        state: String,
        drift: i64,
    },
 
    Downlink
    {
        packet_id: u64,
        reading_count: usize,
        bytes: usize,
        queue_latency: u64,
        payload: String,
    },
 
    Alert
    {
        event: String,
        misses: Option<u32>,
        count: Option<u32>,
        fault_type: Option<String>,
    },
}
 
// Command Message Enum to be deserialised from GCS
#[derive(Serialize, Deserialize, Debug)]
struct GCSCommand
{
    pub tag: String,     
    pub command: String,      // example is "ThermalCheck"
    pub timestamp: u64,       // (ms) simulation time
}
 
// Types of Sensors, FYI Debug = Make it Printable, Clone = Make a copy
#[derive(Debug, Clone, PartialEq)]
enum SensorType
{
    Thermal,
    Accelerometer,
    Gyroscope,
}

// System health state
#[derive(Debug, Clone, PartialEq)]
enum SystemState
{
    Normal,
    Degraded,
    MissionAbort,
}
  

// Process 1 (RAW): Reading off the sensor
#[derive(Debug, Clone)]
struct SensorReading
{
    sensor_type: SensorType,
    sequence: u64,
    value: f64,          // Thermal: temperature, Accelerometer: velocity, Gyroscope: orientation
    drift: i64,
    timestamp: u64,      // (ms) simulation time
    priority: u8,        // lower number = higher priority (1 is most critical)
}

// BinaryHeap Re-ordering as its a MAX-heap by default (prioritizing larger numbers first), flip the comparison so that LOWER numbers (higher priority) come out first
// *follows lecture implementation
impl Ord for SensorReading 
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering
    {
        other.priority.cmp(&self.priority) // Flip 'self' and 'other' to turn Max-Heap into Min-Heap
    }
}

impl PartialOrd for SensorReading 
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> 
    {
        Some(self.cmp(other))
    }
}

impl PartialEq for SensorReading 
{
    fn eq(&self, other: &Self) -> bool 
    {
        self.priority == other.priority
    }
}
impl Eq for SensorReading {}

// Process 2 (COMPRESSION): Serialisable, drops priority, converts SensorType to a plain String so it can be JSON-encoded
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CompressedReading
{
    sensor_type: String,
    sequence: u64,
    value: f64,
    drift: i64,
    timestamp: u64,
}

// Process 3 (PACKETIZATION): Batch of CompressedReadings with packet-level metadata (packet_id, created_at)
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CompressedPayload
{
    packet_id: u64,
    reading_count: usize,
    created_at: u64,
    readings: Vec<CompressedReading>,
}

// Process 4 (QUEUING): Queue entry, holding JSON payload, sits in the downlink queue and gets measured for queue latency
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DataPacket   
{
    packet_id: u64,
    reading_count: usize,
    payload: String,     // serde_json of sensor readings
    created_at: u64,     // (ms) simulation time when packet was queued
    size_bytes: usize,
} 
 

// 3. ---- TYPESTATE - for Radio (Downlink Thread) ----
// With typestates, calling .transmit() when Idle is a compile error, enforcing state machine at compile time, not runtime

// Two possible radio states
struct Idle;
struct Transmitting;
 
// Generic Radio struct - S is the current state
struct Radio<S>
{
    state: PhantomData<S>   // does not store any actual data for S at runtime (zero cost).
}
 
// Methods ONLY when the radio is Idle
impl Radio<Idle>
{
    fn new() -> Self  // Self is a type alias for Radio<Idle>
    {
        Radio 
        { 
            state: PhantomData  // Create a new radio, always starts Idle
        }
    }
 
    // Idle to Transmitting
    fn initialise(self, simulation_start_time: &Instant, log: &Shared<File>) -> Option<Radio<Transmitting>>
    {
        let initialisation_start_time = Instant::now();
        thread::sleep(Duration::from_millis(3));            // simulate hardware initialisation delay
 
        let initialisation_duration = initialisation_start_time.elapsed().as_millis() as u64;
 
        if initialisation_duration > INITIALISE_TIME_LIMIT
        {
            let log_line = format!
            (
                "[{}ms] [DEADLINE] Initialisation {}ms > {}ms — staying Idle",
                simulation_elapsed(simulation_start_time),
                initialisation_duration,
                INITIALISE_TIME_LIMIT
            );
 
            // Deadline violation - shown on console
            append_console_log(log, &log_line);
            None        // Returns None if hardware init exceeds the 5ms hard deadline
        }
        else
        {
            let log_line = format!
            (
                "[{}ms] [RADIO] Initialisation OK ({}ms) - Transmitting",
                simulation_elapsed(simulation_start_time),
                initialisation_duration
            );
 
            append_log(log, &log_line);
 
            Some   // Returns Some  = radio is now Transmitting
            (
                Radio 
                { 
                    state: PhantomData 
                }
            )
        }
    }
}
 
// Methods ONLY when the radio is Transmitting
impl Radio<Transmitting>
{
    // Transmit all queued packets, then return to Idle state.
    fn transmit(self, packets: &[DataPacket], metrics: &Shared<SystemMetrics>, udp_sender: &mpsc::Sender<String>, simulation_start_time: &Instant, log: &Shared<File>) -> Radio<Idle>
    {
        let transmission_start_time = Instant::now();
        let mut transmitted_packet_count = 0usize;
        let mut total_transmitted_bytes = 0usize;
 
        for packet in packets
        {
            if transmission_start_time.elapsed().as_millis() as u64 >= DOWNLINK_TIME_LIMIT
            {
                let violation = format!
                (
                    "[{}ms] [DEADLINE] Downlink window exceeded after {} packets",
                    simulation_elapsed(simulation_start_time),
                    transmitted_packet_count
                );

                append_log(log, &violation);
                append_deadline(metrics, violation);
                break;              // Hard deadline, must finish within the downlink window
            }
 
            // Queue latency = time since this packet was created
            let queue_latency = simulation_elapsed(simulation_start_time).saturating_sub(packet.created_at); // .saturating_subtract() prevents underflow 
            total_transmitted_bytes += packet.size_bytes;
            transmitted_packet_count += 1;
 
            let log_line = format!
            (
                "[{}ms] [DOWNLINK] packet={}  readings={}  {}bytes  queue_latency={}ms",
                simulation_elapsed(simulation_start_time),
                packet.packet_id,
                packet.reading_count,
                packet.size_bytes,
                queue_latency
            );
            append_log(log, &log_line);
 
            // Send downlink metadata to GCS via UDP, through serde
            send_ocs_message
            (
                udp_sender,
                OCSMessage::Downlink
                {
                    packet_id: packet.packet_id,
                    reading_count: packet.reading_count,
                    bytes: packet.size_bytes,
                    queue_latency: queue_latency,
                    payload: packet.payload.clone(),
                }
            );
        }
 
        let transmission_duration = transmission_start_time.elapsed().as_millis() as u64;
 
        let log_line = format!
        (
            "[{}ms] [DOWNLINK] Done  {}/{} packets  {}bytes {}ms",
            simulation_elapsed(simulation_start_time),
            transmitted_packet_count,
            packets.len(),
            total_transmitted_bytes,
            transmission_duration
        );
        append_console_log(log, &log_line);
 
        // Return the Idle state, caller now holds Radio<Idle>
        Radio
        { 
            state: PhantomData
        }
    }
}
 

// 4. ---- SYSTEM METRICS ----

// System Metrics for Final Benchmarking Report
struct SystemMetrics
{
    // Jitter Samples per sensor (μs), where Jitter = |actual - expected|
    thermal_jitter: Vec<i64>,
    accelerometer_jitter: Vec<i64>,
    gyroscope_jitter: Vec<i64>,
 
    // Scheduling drift & buffer insert latency
    drift: Vec<i64>,
    insert_latency: Vec<u64>,
 
    // Recovery & fault tracking
    recovery_times: Vec<u64>,
    dropped_log: Vec<String>,
    deadline_log: Vec<String>,
    fault_log: Vec<String>,
    safety_alerts: Vec<String>,
 
    // Counters
    total_received: u64,
    total_dropped: u64,
    thermal_misses: u32,
    active_time: u64,
    elapsed_time: u64,
}
 
impl SystemMetrics
{
    fn new() -> Self   // Self = SystemMetrics
    {
        SystemMetrics
        {
            // Using Vec::with_capacity()  = no runtime reallocation, pre-allocated worst-case size
            thermal_jitter: Vec::with_capacity(JITTER_MAX_SIZE),
            accelerometer_jitter: Vec::with_capacity(JITTER_MAX_SIZE),
            gyroscope_jitter: Vec::with_capacity(JITTER_MAX_SIZE),
 
            drift: Vec::with_capacity(DRIFT_MAX_SIZE),
            insert_latency: Vec::with_capacity(LATENCY_MAX_SIZE),
 
            recovery_times: Vec::with_capacity(10),
            dropped_log: Vec::with_capacity(LOG_MAX_SIZE),
            deadline_log: Vec::with_capacity(LOG_MAX_SIZE),
            fault_log: Vec::with_capacity(LOG_MAX_SIZE),
            safety_alerts: Vec::with_capacity(LOG_MAX_SIZE),
 
            total_received: 0,
            total_dropped: 0,
            thermal_misses: 0,
            active_time: 0,
            elapsed_time: 0,
        }
    }
}
 
 
// 5. ---- BOUNDED BUFFER ----
// Bounded buffer with Priority for sensor readings
struct PriorityBuffer
{
    heap: BinaryHeap<SensorReading>,  // already reflects priority
    capacity: usize,
}
 
impl PriorityBuffer
{
    fn new(capacity: usize) -> Self
    {
        PriorityBuffer // creates a new PriorityBuffer
        {
            heap: BinaryHeap::with_capacity(capacity),
            capacity,
        }
    }
 
    // Push a reading, returns false (drop) if buffer is at capacity
    fn push(&mut self, reading: SensorReading) -> bool
    {
        if self.heap.len() >= self.capacity
        {
            return false;
        }
 
        self.heap.push(reading);
        true
    }
 
    // Pop the highest priority reading, in order to satisfy deadline
    fn pop(&mut self) -> Option<SensorReading>
    {
        self.heap.pop()
    }
 
    // Calculates how full is the buffer, used to transition to Degraded state
    fn fill_ratio(&self) -> f32
    {
        self.heap.len() as f32 / self.capacity as f32      // calculates length / capacity
    }
}
 

// 6. ---- HELPER METHODS ----

// 6.1 Creates a Shared Arc<Mutex<T>> as repeats many times, (Arc - Atomic Reference Count) = to allow multiple owners, Mutex = thread-safe
type Shared<T> = Arc<Mutex<T>>;

// 6.2 Returns current simulation time from start time
fn simulation_elapsed(simulation_start_time: &Instant) -> u64
{
    simulation_start_time.elapsed().as_millis() as u64
}
 
// 6.3 Boolean helper to determine if system is running
fn is_running(running: &Shared<bool>) -> bool
{
    *running.lock().unwrap()
}
 
// 6.4 Serialise and send a typed OCSMessage through the UDP sender channel
fn send_ocs_message(udp_sender: &mpsc::Sender<String>, message: OCSMessage)
{
    let encoded_message = serde_json::to_string(&message).unwrap_or_default();   // Converts OCSMessage to a JSON string
    let _ = udp_sender.send(encoded_message);  // _ = unused variable, following warning provided
}
 
// 6.5 Append deadline violation into metrics
fn append_deadline(metrics: &Shared<SystemMetrics>, violation: String)
{
    metrics.lock().unwrap().deadline_log.push(violation);
}
 
// 6.6 Calculate scheduling drift
fn calculate_drift(simulation_start_time: &Instant, expected_elapsed: u64) -> i64
{
    let actual_elapsed = simulation_elapsed(simulation_start_time);
    actual_elapsed as i64 - expected_elapsed as i64
}
 
// 6.7 Push a sensor reading into the buffer and record metrics
fn push_record_metrics(priority_buffer: &Shared<PriorityBuffer>, metrics: &Shared<SystemMetrics>, sensor_reading: SensorReading, drift: i64) -> (bool, u64)
{
    // Measure buffer insert latency
    let insert_start_time = Instant::now();
    let accepted = priority_buffer.lock().unwrap().push(sensor_reading);
    let insert_latency = insert_start_time.elapsed().as_micros() as u64;
 
    {   // Update metrics
        let mut metrics_lock = metrics.lock().unwrap();
        metrics_lock.total_received += 1;
        metrics_lock.insert_latency.push(insert_latency);
        metrics_lock.drift.push(drift);
 
        if !accepted
        {
            metrics_lock.total_dropped += 1;  // if didnt fit, dropped
        }
    }
 
    (accepted, insert_latency) // return tuple
}
 
// 6.8 Append log line to the log file only
fn append_log(log: &Shared<File>, line: &str)
{
    let mut log_file = log.lock().unwrap();
    let _ = writeln!(log_file, "{line}");  // _ needed to handle Result, handling warning
}
 
// 6.9 Append important log line to console and log file
fn append_console_log(log: &Shared<File>, line: &str)
{
    println!("{line}");
    append_log(log, line);
}
 
// 6.10 Print a single row in the metrics report table
fn print_row(label: &str, samples: &[i64])
{
    if samples.is_empty()
    {
        println!("|  {:<36} — no data", label); // < means left align
        return;
    }
 
    let minimum_value = samples.iter().min().unwrap();
    let maximum_value = samples.iter().max().unwrap();
    let average_value = samples.iter().sum::<i64>() as f64 / samples.len() as f64;
 
    println!("|  {:<36} n={:>5}  min={:>7}  max={:>7}  avg={:>9.1}", label, samples.len(), minimum_value, maximum_value, average_value);
}
 

// 7. ---- LOG FILE CREATION ----
fn create_log() -> Shared<File>
{
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true) // clear the file
        .open(LOG_FILE)
        .expect("[LOG] Failed to create log file"); // handle error

    Arc::new(Mutex::new(log_file)) // a shared reference is required
}
 
 
// 8. ---- UDP SENDER ----
// Sends OCSMessage telemetry to GCS via UDP
fn udp_sender_thread(udp_receiver: mpsc::Receiver<String>, log: Shared<File>, simulation_start_time: Instant)
{
    // Bind to any available local port for sending
    let socket = UdpSocket::bind("0.0.0.0:0").expect("[OCS-UDP] bind failed");
 
    println!("[OCS] Telemetry link established -> {}", GCS_TELEMETRY_ADDRESS);

    let log_line = format!
    (
        "[{}ms] [OCS-UDP] Ready -> {}",
        simulation_elapsed(&simulation_start_time),
        GCS_TELEMETRY_ADDRESS
    );
    append_log(&log, &log_line);
 
    // for msg in Receiver, the loop ends when all senders are dropped
    for message in &udp_receiver
    {
        socket.send_to(message.as_bytes(), GCS_TELEMETRY_ADDRESS)
            .unwrap_or_else(|error|             // unwrap_or_else = if error
            {
                let log_line = format!
                (
                    "[OCS-UDP] Connection error: {}",
                    error
                );
                append_console_log(&log, &log_line);
                0           // 0 = exit
            });
    }
    
    append_console_log(&log, &format! 
        (
            "[{}ms] [OCS-UDP] All senders dropped — exit.",
            simulation_elapsed(&simulation_start_time)
        ),
    );
}
 

// 9. ---- UDP RECEIVER ----
// Receives commands from the GCS and executes them
fn command_receiver_thread(running: Shared<bool>, emergency: Shared<bool>, log: Shared<File>, simulation_start_time: Instant)
{
    // Bind to the GCS command port
    let socket = UdpSocket::bind(OCS_COMMAND_ADDRESS).expect("[OCS-UDP] bind failed");
    socket.set_read_timeout(Some(Duration::from_millis(UDP_TIMEOUT))).unwrap(); // prevents recv_from from blocking forever,
    
    let log_line = format!
    (
        "[{}ms] [OCS-UDP] Listening on {}",
        simulation_elapsed(&simulation_start_time),
        OCS_COMMAND_ADDRESS
    );
    append_console_log(&log, &log_line);
 
    let mut receive_buffer = [0u8; 4096]; // buffer for incoming packets, 0u8 = unsigned 8-bit integer, 4096 bytes
 
    while is_running(&running)            // timeout allows checking the `running` flag
    {
        match socket.recv_from(&mut receive_buffer) // recv_from = receive from, 
        {
            Ok((received_length, source_address)) =>
            {
                let payload = String::from_utf8_lossy(&receive_buffer[..received_length]);
 
                // serde_json to deserialise the raw bytes into a typed GCSCommand struct
                match serde_json::from_str::<GCSCommand>(&payload)
                {
                    Ok(command) =>
                    {
                        let line = format!(
                            "[{}ms] [OCS-UDP] {} -> command=\"{}\"  timestamp={}",
                            simulation_elapsed(&simulation_start_time),
                            source_address,
                            command.command,
                            command.timestamp
                        );
                        append_log(&log, &line);

                        // Execute the commands received from the GCS
                        match command.command.as_str()
                        {
                            "ThermalCheck" | "AccelerometerCheck" | "GyroscopeCheck" =>
                            {   // GCS is asking sensor health — acknowledge in log (sensors run continuously)
                                append_log(&log, &format!
                                    (
                                        "[{}ms] [OCS-UDP] {} acknowledged — sensor running normally",
                                        simulation_elapsed(&simulation_start_time),
                                        command.command
                                    ));
                            }
                            "RetransmitRequest" =>
                            {   // GCS lost contact — log it 
                                append_console_log(&log, &format!
                                    (
                                        "[{}ms] [OCS-UDP] RetransmitRequest from GCS — loss of contact detected",
                                        simulation_elapsed(&simulation_start_time)
                                    ));
                            }
                            "EmergencyHalt" =>
                            {   // GCS triggered emergency halt (example after MissionAbort alert)
                                append_console_log(&log, &format!
                                    (
                                        "[{}ms] [OCS-UDP] EmergencyHalt received from GCS — initiating shutdown",
                                        simulation_elapsed(&simulation_start_time)
                                    ));
                                *emergency.lock().unwrap() = true;
                                *running.lock().unwrap()   = false;
                            }
                             _ => 
                            {   // Catch-all for unknown commands to satisfy the compiler
                                append_log(&log, &format!
                                (
                                    "[{}ms] [OCS-UDP] Unknown command \"{}\" — ignored",
                                    simulation_elapsed(&simulation_start_time),
                                    command.command
                                ));
                            }
                        }
                    }
                    Err(_) =>
                    {
                        let line = format!
                        (
                            "[{}ms] [OCS-UDP] {} (unparsed): {}",
                            simulation_elapsed(&simulation_start_time),
                            source_address,
                            payload
                        );
                        append_console_log(&log, &line);     // as error
                    }
                }
            }

            // If WouldBlock / TimedOut happens
            Err(error)
                if (error.kind() == std::io::ErrorKind::WouldBlock || error.kind() == std::io::ErrorKind::TimedOut) => 
                {
                    // Just loop again
                }
            // General error
            Err(error) =>
            {
                let line = format!
                (
                    "[{}ms] [OCS-UDP] {}",
                    simulation_elapsed(&simulation_start_time),
                    error
                );
                append_console_log(&log, &line);
            }
        }
    }       // Usage of Result<T, E> enum, sophisticated error handling for Hard RTS
 
    append_console_log(&log, &format!
        (
            "[{}ms] [OCS-UDP] Exit.",
            simulation_elapsed(&simulation_start_time)
        ),
    );
}
 
// ~~~~ SECTION 1: Sensor Data Acquisition & Prioritization ~~~~~
// 10. ---- THERMAL THREAD (CRITICAL SENSOR) ----
fn thermal_thread(priority_buffer: Shared<PriorityBuffer>, metrics: Shared<SystemMetrics>, state: Shared<SystemState>, emergency: Shared<bool>, udp_sender: mpsc::Sender<String>, running: Shared<bool>, log: Shared<File>, simulation_start_time: Instant)
{
    #[cfg(windows)]
    unsafe {winapi::um::processthreadsapi::SetThreadPriority(winapi::um::processthreadsapi::GetCurrentThread(), winapi::um::winbase::THREAD_PRIORITY_HIGHEST as i32,);}

    let log_line = format!
    (
        "[{}ms] [Thermal] period={}ms  buffer_priority=1  CRITICAL SENSOR",
        simulation_elapsed(&simulation_start_time),
        THERMAL_PERIOD
    );
    append_log(&log, &log_line);
    
    let mut sequence_number: u64 = 0;
    let mut last_tick: Instant = Instant::now();
 
    while is_running(&running)
    {
        // Slow down in degraded mode
        let current_state = state.lock().unwrap().clone();
        if current_state == SystemState::Degraded
        {
            thread::sleep(Duration::from_millis(THERMAL_PERIOD * 2));
        }
        else
        {
            thread::sleep(Duration::from_millis(THERMAL_PERIOD));
        }
        let work_start = Instant::now();
        
        // Calculate scheduling drift
        let expected_elapsed = (sequence_number + 1) * THERMAL_PERIOD;
        let drift = calculate_drift(&simulation_start_time, expected_elapsed);
 
        // Simulate a slowly rising temperature
        let temperature: f64 = 20.0 + (sequence_number % 40) as f64; // rises, resets every 40 readings
 
        // Build the sensor reading with priority 1 (highest / most critical)
        let sensor_reading = SensorReading
        {
            sensor_type: SensorType::Thermal,
            sequence: sequence_number,
            value: temperature,
            drift,
            timestamp: simulation_elapsed(&simulation_start_time),
            priority: 1,
        };
        
        // Push the sensor reading to the buffer
        let (accepted, _) = push_record_metrics(&priority_buffer, &metrics, sensor_reading, drift); // _ = insert_latency, not used here
        {
            let mut metrics_lock = metrics.lock().unwrap();
 
            // Record jitter (skip first sequence as there is nothing to deduct from previous)
            if sequence_number == 0
            {
                // First loop has no previous valid interval
                last_tick = Instant::now();
            }
            else
            {
                // Jitter uses microseconds for precision
                let this_interval = last_tick.elapsed().as_micros() as i64;
                let jitter = (this_interval - (THERMAL_PERIOD as i64 * 1000)).unsigned_abs() as i64;
                last_tick = Instant::now();
                metrics_lock.thermal_jitter.push(jitter);
 
                if jitter > THERMAL_JITTER_TIME_LIMIT
                {
                    let line = format!
                    (
                        "[{}ms] [DEADLINE] Thermal jitter {}µs  sequence={}",
                        simulation_elapsed(&simulation_start_time),
                        jitter,
                        sequence_number
                    );
                    append_log(&log, &line);
                    metrics_lock.deadline_log.push(line);
                }
            }
 
            // Check and act for 3 consecutive misses requirement
            if accepted       // no bracket due to warning
            {
                metrics_lock.thermal_misses = 0;
            }
            else
            {
                metrics_lock.thermal_misses += 1;
 
                let fill_ratio = priority_buffer.lock().unwrap().fill_ratio();
                let line = format!
                (
                    "[{}ms] [DROP] Thermal sequence={}  fill={:.1}%", // .1 = 1 decimal place
                    simulation_elapsed(&simulation_start_time),
                    sequence_number,
                    fill_ratio * 100.0      // also 1 decimal place
                );
                append_log(&log, &line);
                metrics_lock.dropped_log.push(line);
 
                // Safety alert IF already 3 consecutive drops
                if metrics_lock.thermal_misses >= THERMAL_MAX_MISSES
                {
                    *emergency.lock().unwrap() = true;
 
                    let line = format!
                    (
                        "[{}ms] !! SAFETY ALERT !! {} thermal misses",
                        simulation_elapsed(&simulation_start_time),
                        metrics_lock.thermal_misses
                    );
                    append_console_log(&log, &line);
                    metrics_lock.safety_alerts.push(line);
 
                    // Send a typed alert to GCS, using serde
                    send_ocs_message
                    (
                        &udp_sender,
                        OCSMessage::Alert 
                        {
                            event: "thermal_alert".into(),
                            misses: Some(metrics_lock.thermal_misses as u32),
                            count: None,            // Not applicable for this fault
                            fault_type: None,    
                        }
                    );
                }
            }
        }
        metrics.lock().unwrap().active_time += work_start.elapsed().as_micros() as u64; 
        sequence_number += 1;
    }
 
    append_console_log(&log, &format!
        (
            "[{}ms] [Thermal] Exit.",
            simulation_elapsed(&simulation_start_time)
        ),
    );
}
 
 
// 11. ---- ACCELEROMETER THREAD ----
fn accelerometer_thread(priority_buffer: Shared<PriorityBuffer>, metrics: Shared<SystemMetrics>, state: Shared<SystemState>, running: Shared<bool>, log: Shared<File>, simulation_start_time: Instant)
{
    #[cfg(windows)]
    unsafe {winapi::um::processthreadsapi::SetThreadPriority(winapi::um::processthreadsapi::GetCurrentThread(), winapi::um::winbase::THREAD_PRIORITY_HIGHEST as i32,);}

    let log_line = format!
    (
        "[{}ms] [Accelerometer] period={}ms  buffer_priority=2",
        simulation_elapsed(&simulation_start_time),
        ACCELEROMETER_PERIOD
    );
    append_log(&log, &log_line);
    
    let mut sequence_number: u64 = 0;
    let mut last_tick: Instant = Instant::now();

    while is_running(&running)
    {
        // Slow down in degraded mode
        let current_state = state.lock().unwrap().clone();
        if current_state == SystemState::Degraded
        {
            thread::sleep(Duration::from_millis(ACCELEROMETER_PERIOD * 2));
        }
        else
        {
            thread::sleep(Duration::from_millis(ACCELEROMETER_PERIOD));
        }
        let work_start = Instant::now();
        
        // Calculate scheduling drift
        let expected_elapsed = (sequence_number + 1) * ACCELEROMETER_PERIOD;
        let drift = calculate_drift(&simulation_start_time, expected_elapsed);

        let magnitude: f64 = 9.81 + (sequence_number % 10) as f64 * 0.05; // simulating small change

        // Build the sensor reading with priority 2
        let sensor_reading = SensorReading
        {
            sensor_type: SensorType::Accelerometer,
            sequence: sequence_number,
            value: magnitude,
            drift,   
            timestamp: simulation_elapsed(&simulation_start_time),
            priority: 2,
        };
        
        // Push the sensor reading to the buffer
        let (accepted, _) = push_record_metrics(&priority_buffer, &metrics, sensor_reading, drift);
        {
            let mut metrics_lock = metrics.lock().unwrap();

            // Record jitter (skip first sequence as there is nothing to deduct from previous)
            if sequence_number == 0
            {
                // First loop has no previous valid interval
                last_tick = Instant::now();
            }
            else
            {
                let this_interval = last_tick.elapsed().as_micros() as i64;
                let jitter = (this_interval - (ACCELEROMETER_PERIOD as i64 * 1000)).unsigned_abs() as i64;
                last_tick = Instant::now();
                metrics_lock.accelerometer_jitter.push(jitter);
            }

            // Log if dropped
            if !(accepted)
            {
                let line = format!
                (
                    "[{}ms] [DROP] Accelerometer sequence={}",
                    simulation_elapsed(&simulation_start_time),
                    sequence_number
                );
                append_log(&log, &line);
                metrics_lock.dropped_log.push(line);
            }
        } 
        metrics.lock().unwrap().active_time += work_start.elapsed().as_micros() as u64; 
        sequence_number += 1;
    }

    append_console_log(&log, &format!
        (
            "[{}ms] [Accelerometer] Exit.",
            simulation_elapsed(&simulation_start_time)
        ),
    );
}

 
// 12. ---- GYROSCOPE THREAD ----
fn gyroscope_thread(priority_buffer: Shared<PriorityBuffer>, metrics: Shared<SystemMetrics>, state: Shared<SystemState>, running: Shared<bool>, log: Shared<File>, simulation_start_time: Instant)
{
    #[cfg(windows)]
    unsafe {winapi::um::processthreadsapi::SetThreadPriority(winapi::um::processthreadsapi::GetCurrentThread(), winapi::um::winbase::THREAD_PRIORITY_HIGHEST as i32,);}

    let log_line = format!
    (
        "[{}ms] [Gyroscope] period={}ms  buffer_priority=3",
        simulation_elapsed(&simulation_start_time),
        GYROSCOPE_PERIOD
    );
    append_log(&log, &log_line);
    
    let mut sequence_number: u64 = 0;
    let mut last_tick: Instant = Instant::now();

    while is_running(&running)
    {
        // Slow down in degraded mode
        let current_state = state.lock().unwrap().clone();
        if current_state == SystemState::Degraded
        {
            thread::sleep(Duration::from_millis(GYROSCOPE_PERIOD * 2));
        }
        else
        {
            thread::sleep(Duration::from_millis(GYROSCOPE_PERIOD));
        }
        let work_start = Instant::now();
        
        // Calculate scheduling drift
        let expected_elapsed = (sequence_number + 1) * GYROSCOPE_PERIOD;
        let drift = calculate_drift(&simulation_start_time, expected_elapsed);
        let orientation: f64 = (sequence_number % 360) as f64; // cycles full rotations

        // Build the sensor reading with priority 3 (lowest priority)
        let sensor_reading = SensorReading
        {
            sensor_type: SensorType::Gyroscope,
            sequence: sequence_number,
            value: orientation,
            drift,
            timestamp: simulation_elapsed(&simulation_start_time),
            priority: 3,
        };
        
        // Push the sensor reading to the buffer
        let (accepted, _) = push_record_metrics(&priority_buffer, &metrics, sensor_reading, drift);
        {
            let mut metrics_lock = metrics.lock().unwrap();

            // Record jitter (skip first sequence as there is nothing to deduct from previous)
            if sequence_number == 0
            {
                // First loop has no previous valid interval
                last_tick = Instant::now();
            }
            else
            {
                let this_interval = last_tick.elapsed().as_micros() as i64;
                let jitter = (this_interval - (GYROSCOPE_PERIOD as i64 * 1000)).unsigned_abs() as i64;
                last_tick = Instant::now();
                metrics_lock.gyroscope_jitter.push(jitter);
            }

            // Log if dropped
            if !(accepted)
            {
                let line = format!
                (
                    "[{}ms] [DROP] Gyroscope sequence={}",
                    simulation_elapsed(&simulation_start_time),
                    sequence_number
                );
                append_log(&log, &line);
                metrics_lock.dropped_log.push(line);
            }
        }
        metrics.lock().unwrap().active_time += work_start.elapsed().as_micros() as u64; 
        sequence_number += 1;
    }

    append_console_log(&log, &format!
        (
            "[{}ms] [Gyroscope] Exit.",
            simulation_elapsed(&simulation_start_time)
        ),
    );
}


// ~~~~ SECTION 2: Real-Time Task Scheduling  ~~~~~
// 13. ---- HEALTH MONITOR THREAD (Rate Monotonic P1) ----
// Reports buffer fill, system state, and drift every 200ms, sending OCSMessage::Status
fn health_monitor_task(priority_buffer: Shared<PriorityBuffer>, metrics: Shared<SystemMetrics>, state: Shared<SystemState>, udp_sender: mpsc::Sender<String>, running: Shared<bool>, log: Shared<File>, simulation_start_time: Instant) -> impl FnMut() + Send + 'static
{
    let mut iteration: u64 = 0;
    
    move ||
    {
        // If not running, return
        if !is_running(&running)
        {
            return;
        }
        let active_work_start_time = Instant::now();
 
        // Calculate scheduling drift
        let expected_elapsed = iteration * HEALTH_MONITOR_PERIOD;
        let drift = calculate_drift(&simulation_start_time,expected_elapsed,);
        
        // Get buffer state
        let (fill_ratio, buffer_length) =
        {
            let buffer_lock = priority_buffer.lock().unwrap();
            (buffer_lock.fill_ratio(), buffer_lock.heap.len())
        };

        // degraded-mode transition based on priority buffer fill
        {
            let mut system_state_lock = state.lock().unwrap();

            if *system_state_lock != SystemState::MissionAbort
            {
                if fill_ratio >= DEGRADED_MODE
                {
                    if *system_state_lock == SystemState::Normal
                    {
                        *system_state_lock = SystemState::Degraded;

                        let line = format!
                        (
                            "[{}ms] [Health] Buffer >= 80% -> DEGRADED",
                            simulation_elapsed(&simulation_start_time)
                        );
                        append_console_log(&log, &line);
                    }
                }
                else
                {
                    if *system_state_lock == SystemState::Degraded
                    {
                        *system_state_lock = SystemState::Normal;

                        let line = format!
                        (
                            "[{}ms] [Health] Buffer recovered -> NORMAL",
                            simulation_elapsed(&simulation_start_time)
                        );
                        append_console_log(&log, &line);
                    }
                }
            }
        }
        // Get system state
        let system_state = state.lock().unwrap().clone();
 
        let log_line = format!
        (
            "[{}ms] [Health] iteration={:>4}  buffer={}/{} ({:.1}%)  state={:?}  drift={:+}ms",
            simulation_elapsed(&simulation_start_time),
            iteration,
            buffer_length,
            PRIORITY_BUFFER,
            fill_ratio * 100.0,
            system_state,
            drift
        );
        append_log(&log, &log_line);
 
        // Flag if health task itself is drifting
        if drift.abs() > 5  // abs = absolute because drift is +ve or -ve, 5ms check (Additional)
        {
            let violation = format!
            (
                "[{}ms] [WARN] Health drift={:+}ms iteration={}",
                simulation_elapsed(&simulation_start_time),
                drift,
                iteration
            );
            append_log(&log, &violation);
        }
 
        // Send telemetry to GCS
        send_ocs_message
        (
            &udp_sender,
            OCSMessage::Status
            {
                iteration: iteration,
                fill: fill_ratio as f64 * 100.0,
                state: format!("{system_state:?}"), // ? = enum to string
                drift: drift,
            }
        );
 
        let active_work_duration = active_work_start_time.elapsed().as_millis() as u64;
        {   // Update metrics
            let mut metrics_lock = metrics.lock().unwrap();
            metrics_lock.drift.push(drift);
            metrics_lock.active_time += active_work_duration;
            metrics_lock.elapsed_time = simulation_elapsed(&simulation_start_time);
        }
 
        iteration += 1;
    }
}


// 14. ---- DATA COMPRESSION THREAD (Rate Monotonic P2) ----
// Drains up to 20 readings from the priority buffer, compresses with 50% size reduction, enqueues a DataPacket for downlink
fn data_compression_task(priority_buffer: Shared<PriorityBuffer>, downlink_queue: Shared<Vec<DataPacket>>, metrics: Shared<SystemMetrics>, running: Shared<bool>, log: Shared<File>, simulation_start_time: Instant) -> impl FnMut() + Send + 'static
{
    let mut packet_id: u64 = 0;
    let mut iteration: u64 = 0;
 
    move ||
    {
        // If not running, return
        if !is_running(&running)
        {
            return;
        }
        let active_work_start_time = Instant::now();
 
        // Calculate scheduling drift
        let expected_elapsed = iteration * DATA_COMPRESSION_PERIOD;
        let drift = calculate_drift(&simulation_start_time, expected_elapsed);
 
        // Drain up to 20 items with .pop()
        let mut sensor_batch: Vec<SensorReading> = Vec::new();
        {
            let mut buffer_lock = priority_buffer.lock().unwrap();
 
            for _ in 0..20  // _ = throw away variable as warning
            {
                match buffer_lock.pop()
                {
                    Some(sensor_reading) => sensor_batch.push(sensor_reading),  // push to new batch
                    None => break,      // if buffer is now empty, break
                }
            }
        }
 
        if !sensor_batch.is_empty()
        {
            // Build a typed compressed payload
            let compressed_readings: Vec<CompressedReading> = sensor_batch
                .iter()
                .map(|sensor_reading| CompressedReading     // maps sensor_reading to CompressedReading
                {
                    sensor_type: format!("{:?}", sensor_reading.sensor_type),  // ? = enum to string
                    sequence: sensor_reading.sequence,
                    value: sensor_reading.value,
                    drift: sensor_reading.drift,
                    timestamp: sensor_reading.timestamp,
                })
                .collect();

            let compressed_payload = CompressedPayload // previous maps array to CompressedPayload
            {
                packet_id,
                reading_count: compressed_readings.len(),
                created_at: simulation_elapsed(&simulation_start_time),
                readings: compressed_readings,
            };

            // Serialize compressed payload
            let payload = serde_json::to_string(&compressed_payload).unwrap();
 
            // Simulated 50% compression ratio, rounding down to at least 1 byte 
            let compressed_size_bytes = (payload.len() / 2).max(1);
 
            // Measure queue insert latency, push to downlink queue
            let queue_insert_start_time = Instant::now();
            downlink_queue.lock().unwrap().push(DataPacket
            {
                packet_id,
                reading_count: sensor_batch.len(),
                payload: payload,
                created_at: simulation_elapsed(&simulation_start_time),
                size_bytes: compressed_size_bytes,
            });
            
            // Calculate queue insert latency and log
            let queue_insert_latency = queue_insert_start_time.elapsed().as_micros() as u64;
            let line = format!
            (
                "[{}ms] [OCS-Compress] packet={}  n={}  {}bytes   queue_insert latency={}µs  drift={:+}ms",
                simulation_elapsed(&simulation_start_time),
                packet_id,
                sensor_batch.len(),
                compressed_size_bytes,
                queue_insert_latency,
                drift
            );
            append_log(&log, &line);
 
            packet_id += 1;
        }
 
        if drift.abs() > 10 // 10 ms check (Additional)
        {
            let violation = format!
            (
                "[{}ms] [WARN] Compress drift={:+}ms iteration={}",
                simulation_elapsed(&simulation_start_time),
                drift,
                iteration
            );
            append_log(&log, &violation);
        }
 
        {   // Update metrics
            let mut metrics_lock = metrics.lock().unwrap();
            metrics_lock.drift.push(drift);
            metrics_lock.active_time += active_work_start_time.elapsed().as_millis() as u64;
        }
 
        iteration += 1;
    }
}


// 15. ---- ANTENNA ALIGNMENT THREAD (Rate Monotonic P3) ----
// Lowest priority RM task, therefore can be preempted (skipped) when:
// emergency=true OR system is in MissionAbort state — demonstrating priority-based preemption
fn antenna_alignment_task(metrics: Shared<SystemMetrics>, state: Shared<SystemState>, emergency: Shared<bool>, running: Shared<bool>, log: Shared<File>, simulation_start_time: Instant) -> impl FnMut() + Send + 'static
{
    let mut iteration: u64 = 0;
 
    move ||
    {
        // If not running, return
        if !is_running(&running)
        {
            return;
        }
 
        // Preemption check: skip if emergency or abort (Emergency = caused by Thermal Control, Abort = caused by Fault Injection)
        if *emergency.lock().unwrap() || *state.lock().unwrap() == SystemState::MissionAbort
        {
            // Append to console, log (metrics)
            let log_line = format!
            (
                "[{}ms] [Antenna] iteration={:>4}  PREEMPTED",
                simulation_elapsed(&simulation_start_time),
                iteration
            );
            append_console_log(&log, &log_line);
 
            iteration += 1;
            return;
        }
 
        // Compute scheduling drift
        let active_work_start_time = Instant::now();
        let expected_elapsed = iteration * ANTENNA_ALIGNMENT_PERIOD;
        let drift = calculate_drift(&simulation_start_time, expected_elapsed);
 
        // Compute angle and elevation for this iteration
        let angle_degrees = (iteration as f64 * 7.2) % 360.0; // full 360 degrees every 50 iterations
        let elevation_degrees = 30.0 + 20.0 * (iteration as f64 * 0.05).sin();
 
        let line = format!
        (
            "[{}ms] [Antenna] iteration={:>4}  angle={:>6.1}degrees  elevation={:>5.1}degrees  drift={:+}ms",
            simulation_elapsed(&simulation_start_time),
            iteration,
            angle_degrees,
            elevation_degrees,
            drift
        );
        append_log(&log, &line);
 
        if drift.abs() > 5 // 5 ms check
        {
            let violation = format!
            (
                "[{}ms] [WARN] Antenna drift={:+}ms iteration={}",
                simulation_elapsed(&simulation_start_time),
                drift,
                iteration
            );
            append_log(&log, &violation);
        }
 
        {   // Update metrics
            let mut metrics_lock = metrics.lock().unwrap();
            metrics_lock.drift.push(drift);
            metrics_lock.active_time += active_work_start_time.elapsed().as_millis() as u64;
        }
 
        iteration += 1;
    }
}
 

// ~~~~ SECTION 3: Downlink Data Management ~~~~~
// 16. ---- DOWNLINK THREAD ----
fn downlink_thread(downlink_queue: Shared<Vec<DataPacket>>, metrics: Shared<SystemMetrics>, udp_sender: mpsc::Sender<String>, running: Shared<bool>, log: Shared<File>, simulation_start_time: Instant) 
{
    let log_line = format!
    (
        "[{}ms] [Downlink] visibility={}s  window={}ms  (Typestate Active)",
        simulation_elapsed(&simulation_start_time),
        VISIBILITY_PERIOD,
        DOWNLINK_TIME_LIMIT
    );
    append_log(&log, &log_line);
    

    while is_running(&running)
    {
        // Wait for next visibility window
        thread::sleep(Duration::from_millis(VISIBILITY_PERIOD));
        let work_start = Instant::now();
        if !is_running(&running)
        {
            break;
        }
 
        let line = format!
        (
            "[{}ms] [Downlink] ~~~ Visibility Window ~~~",
            simulation_elapsed(&simulation_start_time)
        );
        append_log(&log, &line);
 
        // Typestate transition = Radio<Idle> -> Radio<Transmitting>
        let radio = Radio::<Idle>::new();
 
        match radio.initialise(&simulation_start_time, &log)
        {
            None =>
            {
                // Initialisation failed (exceeded 5ms deadline)
                let violation = format!(
                    "[{}ms] [DEADLINE] Radio initialisation exceeded {}ms",
                    simulation_elapsed(&simulation_start_time),
                    INITIALISE_TIME_LIMIT
                );
                append_console_log(&log, &violation);
                append_deadline(&metrics, violation);
            }
 
            Some(transmitting_radio) =>
            {
                // Drain the queue under a brief lock, releases it, then transmits, Hard RTS pattern of minimise lock hold time
                let queued_packets: Vec<DataPacket> = downlink_queue.lock().unwrap().drain(..).collect(); // .. = all elements

                if queued_packets.is_empty()
                {
                    let line = format!
                    (
                        "[{}ms] [Downlink] Nothing queued.",
                        simulation_elapsed(&simulation_start_time)
                    );
 
                    append_log(&log, &line);
                }
 
                // transmit() changes Radio<Transmitting> and returns Radio<Idle>
                let _ = transmitting_radio.transmit(&queued_packets, &metrics, &udp_sender, &simulation_start_time, &log);
                metrics.lock().unwrap().active_time += work_start.elapsed().as_micros() as u64;    // update metrics
            }
        }
    }
 
    append_console_log(&log,&format!
        (
            "[{}ms] [Downlink] Exit.",
            simulation_elapsed(&simulation_start_time)
        ),
    );
}
 

// ~~~~ SECTION 4: BENCHMARKING AND FAULT SIMULATION ~~~~~
// 17. ---- FAULT INJECTOR ----
fn fault_injector_thread(metrics: Shared<SystemMetrics>, state: Shared<SystemState>, emergency: Shared<bool>, udp_sender: mpsc::Sender<String>, running: Shared<bool>, log: Shared<File>, simulation_start_time: Instant)
{
    let log_line = format!
    (
        "[{}ms] [Faults] interval={}s  recovery_limit={}ms type=CorruptedReading",  // only 1 type of fault
        simulation_elapsed(&simulation_start_time),
        FAULT_INJECTION_PERIOD,
        RECOVERY_TIME_LIMIT / 1000
    );
    append_log(&log, &log_line);
    
    // Fault injection counter
    let mut fault_count: u32 = 0;

    while is_running(&running) && fault_count < 2
    {
        thread::sleep(Duration::from_secs(FAULT_INJECTION_PERIOD));
        let work_start = Instant::now();

        if !is_running(&running)
        {
            break;
        }

        fault_count += 1;

        let fault_line = format!
        (
            "[{}ms] [FAULT #{}] CorruptedReading",
            simulation_elapsed(&simulation_start_time),
            fault_count
        );
        append_console_log(&log, &fault_line);

        {
            // Update metrics
            let mut metrics_lock = metrics.lock().unwrap();
            metrics_lock.fault_log.push(fault_line);
        }

        // Send a typed alert to GCS, using serde
        send_ocs_message
        (
            &udp_sender,
            OCSMessage::Alert 
            {
                event: "fault".into(),
                misses: None,           // Not applicable for this fault
                count: Some(fault_count),            
                fault_type: Some("CorruptedReading".into()),     
            }
        );

        let response_line = format!
        (
            "[{}ms] [Faults] Discarding corrupted reading and recovering...",
            simulation_elapsed(&simulation_start_time)
        );
        append_console_log(&log, &response_line);

        // Measure recovery time
        let recovery_start_time = Instant::now();
        thread::sleep(Duration::from_millis(50)); // simulate recovery, 50 ms

        let recovery_time = recovery_start_time.elapsed().as_micros() as u64; // will be 50ms +- jitter
        let recovery_done_line = format!
        (
            "[{}ms] [Faults] Recovery in {}ms",
            simulation_elapsed(&simulation_start_time),
            recovery_time
        );
        append_console_log(&log, &recovery_done_line);

        {   // Update metrics
            let mut metrics_lock = metrics.lock().unwrap();
            metrics_lock.recovery_times.push(recovery_time);

            if recovery_time > RECOVERY_TIME_LIMIT
            {
                let abort_message = format!
                (
                    "[{}ms] !! MISSION ABORT !! recovery {}ms > {}ms limit",
                    simulation_elapsed(&simulation_start_time),
                    recovery_time,
                    RECOVERY_TIME_LIMIT / 1000
                );
                append_console_log(&log, &abort_message);
                metrics_lock.safety_alerts.push(abort_message);

                // Set emergency so Antenna Alignment Task is preempted immediately
                *emergency.lock().unwrap() = true;
                *state.lock().unwrap() = SystemState::MissionAbort;

                // Notify GCS so it can send EmergencyHalt back
                send_ocs_message
                (
                    &udp_sender,
                    OCSMessage::Alert
                    {
                        event: "mission_abort".into(),
                        misses: None,
                        count: Some(fault_count),
                        fault_type: Some("RecoveryTimeout".into()),
                    }
                );
            }
        }
        metrics.lock().unwrap().active_time += work_start.elapsed().as_micros() as u64;  // update metrics
    }

    append_console_log(&log, &format!
        (
            "[{}ms] [Faults] Exit.",
            simulation_elapsed(&simulation_start_time)
        ),
    );
}
 

// ~~~~ SECTION 5: FINAL BENCHMARKING REPORT ~~~~~
// 18. ---- FINAL BENCHMARKING REPORT ----
fn print_final_report(metrics: &SystemMetrics)
{
    // Calculate packet loss percentage
    let packet_loss;
    if metrics.total_received > 0
    {
        packet_loss = metrics.total_dropped as f64 / metrics.total_received as f64 * 100.0;
    }
    else
    {
        packet_loss = 0.0;
    }

    // Calculate approximate CPU utilisation
    let cpu_utilisation;
    if metrics.elapsed_time > 0
    {
        cpu_utilisation = metrics.active_time as f64 / (metrics.elapsed_time as f64 * 1000.0) * 100.0;
    }
    else
    {
        cpu_utilisation = 0.0;
    }
 
    println!("\n~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~");
    println!("|              OCS FINAL BENCHMARKING REPORT - BY LUVEN MARK (TP071542)                        |");
    println!("~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~");
    println!("|  Sensor Readings Received: {}  Sensor Readings Dropped: {} ({:.2}%)", metrics.total_received, metrics.total_dropped, packet_loss); // .2 = 2 decimal places
    println!("|  Jitter (µs)  limit={}µs", THERMAL_JITTER_TIME_LIMIT);
    print_row("Thermal [CRITICAL]", &metrics.thermal_jitter);
    print_row("Accelerometer", &metrics.accelerometer_jitter);
    print_row("Gyroscope", &metrics.gyroscope_jitter);
    println!("|  Drift (ms)");
    print_row("All tasks", &metrics.drift);
    println!("|  Insert latency (µs)");
    let insert_latency_values: Vec<i64> = metrics.insert_latency.iter().map(|&value| value as i64).collect(); // it iterates over the values and maps them to i64, then collects the resulting vector
    print_row("priority_buffer.push()", &insert_latency_values);
    println!("|  Deadline violations: {}", metrics.deadline_log.len());

    for violation in metrics.deadline_log.iter().take(5) // for each violation in deadline log, proceed to print but only the first 5
    {
        println!("|    {violation}");
    }
    if metrics.deadline_log.len() > 5
    {
        println!("|    ... and {} more", metrics.deadline_log.len() - 5 // print the number of violations minus the 5 already printed
        );
    }

    println!("|  Faults: {}", metrics.fault_log.len()); // print the number of faults

    for fault in &metrics.fault_log
    {
        println!("|    {fault}"); // print each fault
    }
    if !metrics.recovery_times.is_empty()
    {
        let maximum_recovery_time = metrics.recovery_times.iter().max().unwrap(); // get the maximum recovery time
        let average_recovery_time = metrics.recovery_times.iter().sum::<u64>() as f64 / metrics.recovery_times.len() as f64; // formula = sum of recovery times / number of recovery times
        println!("|    recovery max={}µs  avg={}µs", maximum_recovery_time, average_recovery_time);
    }
 
    println!("|  CPU ≈ {:.2}%", cpu_utilisation);
 
    if !metrics.safety_alerts.is_empty()
    {
        println!("|  SAFETY ALERTS:");
        for alert in &metrics.safety_alerts
        {
            println!("|    {alert}"); // print each alert
        }
    }
    println!("~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~\n");
}

#[cfg(windows)]
fn s() { unsafe {winapi::um::timeapi::timeBeginPeriod(1);}}
#[cfg(not(windows))]
fn s() {}
// ~~~~ SECTION 6: MAIN FUNCTION ~~~~~
// 18. ---- MAIN ----
// Sets up all shared state, spawns all OS threads, schedules RM background tasks via ScheduledThreadPool
// In the end, shuts everything down gracefully and prints final benchmarking report
fn main()
{
    s();
    println!("~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~");
    println!("|  SATELLITE ONBOARD CONTROL SYSTEM (OCS) - BY LUVEN MARK (TP071542) |");
    println!("|  TYPE: HARD RTOS, demonstrating the learnt Hard RTS concepts.      |");
    println!("~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~");

    // Initialization
    let simulation_start_time: Instant = Instant::now();
    let log: Shared<File> = create_log();
    append_log(&log, "[OCS] Startup sequence initiated.");

    // Creation of Shared State
    let shared_priority_buffer: Shared<PriorityBuffer> = Arc::new(Mutex::new(PriorityBuffer::new(PRIORITY_BUFFER)));
    let shared_system_metrics: Shared<SystemMetrics> = Arc::new(Mutex::new(SystemMetrics::new()));
    let shared_system_state: Shared<SystemState> = Arc::new(Mutex::new(SystemState::Normal));
    let shared_downlink_queue: Shared<Vec<DataPacket>> = Arc::new(Mutex::new(Vec::<DataPacket>::with_capacity(25))); // 25 = 5,000ms / 200ms  (Visibility Window / Packet Period) = 25 packets 
    let shared_emergency_flag: Shared<bool> = Arc::new(Mutex::new(false));
    let shared_running_flag: Shared<bool> = Arc::new(Mutex::new(true));

    // UDP Channel
    let (telemetry_sender, telemetry_receiver) = mpsc::channel::<String>();

    // Configuration on Console
    println!("[OCS] Configuration");
    println!("~~~~~~~~~~~~~~~~~~~");
    println!(" Telemetry -> GCS : {GCS_TELEMETRY_ADDRESS}");
    println!(" Commands  <- GCS : {OCS_COMMAND_ADDRESS}");
    println!(" Buffer capacity  : {PRIORITY_BUFFER}");
    println!(" Typestate (Radio): Idle -> Transmitting");
    println!(" RM pool tasks    : health / compress / antenna");
    println!(" Sim duration     : {SIMULATION_DURATION}s\n");
    append_log(&log, "[OCS] System configuration completed.");

    // Operating System (OS) Thread 
    let mut os_thread_handles: Vec<thread::JoinHandle<()>> = Vec::new(); // JoinHandle =  used to wait for the thread to finish

    // UDP sender thread
    os_thread_handles.push(thread::spawn
    ({
        let logger = Arc::clone(&log);
        let start  = simulation_start_time;
        let receiver = telemetry_receiver;
        move || udp_sender_thread(receiver, logger, start) // move ownership and start to the thread
    }));

    // UDP receiver thread
    os_thread_handles.push(thread::spawn
    ({
        let running = Arc::clone(&shared_running_flag);  // need to clone as the Arc is shared
        let emergency = Arc::clone(&shared_emergency_flag);
        let logger = Arc::clone(&log);
        let start = simulation_start_time;
        move || command_receiver_thread(running, emergency, logger, start)
    }));

    // SENSOR 1: Thermal thread
    os_thread_handles.push(thread::spawn
    ({
        let buffer = Arc::clone(&shared_priority_buffer);
        let metrics = Arc::clone(&shared_system_metrics);
        let state = Arc::clone(&shared_system_state);
        let emergency = Arc::clone(&shared_emergency_flag);
        let transmit = telemetry_sender.clone();
        let running = Arc::clone(&shared_running_flag);
        let logger = Arc::clone(&log);
        let start = simulation_start_time;
        move || thermal_thread(buffer, metrics, state, emergency, transmit, running, logger, start)
    }));

    // SENSOR 2: Accelerometer thread
    os_thread_handles.push(thread::spawn
    ({
        let buffer = Arc::clone(&shared_priority_buffer);
        let metrics = Arc::clone(&shared_system_metrics);
        let state = Arc::clone(&shared_system_state); 
        let running = Arc::clone(&shared_running_flag);
        let logger = Arc::clone(&log);
        let start = simulation_start_time;
        move || accelerometer_thread(buffer, metrics, state, running, logger, start)
    }));

    // SENSOR 3: Gyroscope thread
    os_thread_handles.push(thread::spawn
    ({
        let buffer = Arc::clone(&shared_priority_buffer);
        let metrics = Arc::clone(&shared_system_metrics);
        let state = Arc::clone(&shared_system_state);
        let running = Arc::clone(&shared_running_flag);
        let logger = Arc::clone(&log);
        let start = simulation_start_time;
        move || gyroscope_thread(buffer, metrics, state, running, logger, start)
    }));

    // ScheduledThreadPool for 3 Rate Monotonic Tasks
    let rate_monotonic_thread_pool = ScheduledThreadPool::new(3);

    rate_monotonic_thread_pool.execute_at_fixed_rate // fixed rate means that the task will be executed at fixed intervals
    (
        Duration::from_millis(0),               // start after 0ms
        Duration::from_millis(HEALTH_MONITOR_PERIOD),
        health_monitor_task
        (
            Arc::clone(&shared_priority_buffer),
            Arc::clone(&shared_system_metrics),
            Arc::clone(&shared_system_state),
            telemetry_sender.clone(),
            Arc::clone(&shared_running_flag),
            Arc::clone(&log),
            simulation_start_time,
        ),
    );

    rate_monotonic_thread_pool.execute_at_fixed_rate
    (
        Duration::from_millis(0),
        Duration::from_millis(DATA_COMPRESSION_PERIOD),
        data_compression_task
        (
            Arc::clone(&shared_priority_buffer),
            Arc::clone(&shared_downlink_queue),
            Arc::clone(&shared_system_metrics),
            Arc::clone(&shared_running_flag),
            Arc::clone(&log),
            simulation_start_time,
        ),
    );

    rate_monotonic_thread_pool.execute_at_fixed_rate
    (
        Duration::from_millis(0),
        Duration::from_millis(ANTENNA_ALIGNMENT_PERIOD),
        antenna_alignment_task
        (
            Arc::clone(&shared_system_metrics),
            Arc::clone(&shared_system_state),
            Arc::clone(&shared_emergency_flag),
            Arc::clone(&shared_running_flag),
            Arc::clone(&log),
            simulation_start_time,
        ),
    );

    // Downlink thread
    os_thread_handles.push(thread::spawn
    ({
        let downlink_queue = Arc::clone(&shared_downlink_queue);
        let metrics = Arc::clone(&shared_system_metrics);
        let transmit = telemetry_sender.clone();
        let running = Arc::clone(&shared_running_flag);
        let logger = Arc::clone(&log);
        let start = simulation_start_time;
        move || downlink_thread(downlink_queue, metrics, transmit, running, logger, start)
    }));

    // Fault injector thread
    os_thread_handles.push(thread::spawn
    ({
        let metrics = Arc::clone(&shared_system_metrics);
        let state = Arc::clone(&shared_system_state);
        let emergency = Arc::clone(&shared_emergency_flag);
        let transmit = telemetry_sender.clone();
        let running = Arc::clone(&shared_running_flag);
        let logger = Arc::clone(&log);
        let start = simulation_start_time;
        move || fault_injector_thread(metrics, state, emergency, transmit, running, logger, start)
    }));

    // Drop the final sender so udp_sender_thread can exit cleanly
    drop(telemetry_sender);

    println!
    (
        "[OCS] {} OS threads + 3 ScheduledThreadPool tasks online. Running for {}s...\n",
        os_thread_handles.len(),
        SIMULATION_DURATION
    );
    append_log(&log, "[OCS] All OCS threads and RM tasks are online.");

    // Run for the Simulation Duration
    thread::sleep(Duration::from_secs(SIMULATION_DURATION));

    // Version of Graceful Shutdown with Running Flag (Hard RTS)
    println!("\n[OCS] Simulation ended — signalling shutdown...");
    append_log(&log, "[OCS] Simulation duration elapsed. Shutdown initiated.");

    *shared_running_flag.lock().unwrap() = false;   // signal shutdown with running flag set to false

    // Drop the rate monotonic thread pool so it can exit cleanly
    drop(rate_monotonic_thread_pool);

    // Wait for all threads to join (all threads will need to complete before the main thread exits)
    for handle in os_thread_handles
    {
        let _ = handle.join();
    }
    append_log(&log, "[OCS] All OCS threads joined successfully.");

    // Final Benchmarking Report 
    println!("\n[OCS] FINAL BENCHMARKING REPORT:");
    {   // Lock and print final benchmarking report
        let metrics_lock = shared_system_metrics.lock().unwrap();
        print_final_report(&metrics_lock);
    }

    append_log(&log, "[OCS] Final benchmarking report generated.");
    println!("[OCS] System Shutdown Complete.");    // Main Thread exits
}