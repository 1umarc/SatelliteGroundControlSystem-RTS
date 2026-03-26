// GROUND CONTROL STATION - BY CHONG CHUN KIT (TP077436)
// TYPE: SOFT RTS, demonstrating the learnt Soft RTS concepts

// Standard library stuff we need
use std::fs::{File, OpenOptions};           // for creating and writing to the log file
use std::io::Write;                         // lets us write lines into the log file
use std::marker::PhantomData;               // used for the typestate pattern 
use std::sync::{Arc, Mutex};                // Arc = shared ownership, Mutex = safe shared access
use std::time::{Duration, Instant};         // Instant for measuring elapsed time in the simulation

// Tokio async imports
use tokio::net::UdpSocket;                  // async UDP so we don't block while waiting for packets
use tokio::sync::mpsc;                      // async channel — lets tasks pass data between each other
use tokio::time::sleep;                     // async sleep, won't block the whole thread
use tokio_util::sync::CancellationToken;    // used to signal all tasks to shut down cleanly

// External crates
use serde::{Deserialize, Serialize};        // auto JSON serialise/deserialise for UDP messages
use serde_json;                             // the actual JSON encoding/decoding

// ========================
// Rate Monotonic Command Periods (milliseconds)
// Use all caps for constants to follow snake_case
// Shorter period = higher priority under Rate Monotonic scheduling
const THERMAL_COMMAND_PERIOD: u64 = 200;   // RM Priority 1 — fastest, so highest priority
const ACCELEROMETER_COMMAND_PERIOD: u64 = 400;  // RM Priority 2
const GYROSCOPE_COMMAND_PERIOD: u64 = 600;  // RM Priority 3 — slowest, so lowest priority

// Deadline limits from the assignment spec
const DECODE_DEADLINE: u64 = 3;    // telemetry must be decoded within 3ms
const DISPATCH_DEADLINE: u64 = 2;    // urgent commands must be dispatched within 2ms
const FAULT_RESPONSE_LIMIT: u64 = 100;  // interlock must engage within 100ms of a fault

// Loss of contact threshold
const LOSS_OF_CONTACT_MISS_THRESHOLD: u32 = 3;
// if we miss 3 or more packets in a row, we declare loss of contact

const REREQUEST_INTERVAL: u64 = 500;
// check for silence every 500ms — quick enough to notice, not so fast we spam requests

// Jitter warning limit in microseconds
const UPLINK_JITTER_LIMIT: i64 = 3000;
// 3000µs = 3ms, similar to the dispatch deadline (additional)

// How long the whole simulation runs
const SIMULATION_DURATION: u64 = 125;  // 2.08 minutes, same as OCS

// Network addresses
const GCS_TELEMETRY_BIND: &str = "0.0.0.0:9000";   // GCS listens for OCS telemetry on this port
const OCS_COMMAND_ADDRESS: &str = "127.0.0.1:9001";  // GCS sends commands to OCS here

// Log file name
const LOG_FILE: &str = "gcs.log"; 
// all the detailed per-packet events go here so the terminal doesn't get too noisy

// ===============
// Messages that the OCS sends to us over UDP.
// Each variant maps to one type of telemetry or alert.
// Field names must exactly match what the OCS serialises —
// serde_json silently fails to parse any packet where a name differs.
//
// The OCS enum has only #[derive(Serialize, Deserialize, Debug)] with NO serde tag attribute.
// That means it uses Rust's default externally-tagged format, which looks like:
//   {"Thermal":{"sequence":1,"temperature":36.5,"drift":2}}
//   {"Alert":{"event":"fault","misses":null,"count":1,"fault_type":"CorruptedReading"}}
// This GCS enum must match that format exactly — no #[serde(tag)] here.
// CHANGED: removed #[serde(tag = "tag", rename_all = "snake_case")] — that format
//          does NOT match what the OCS sends and caused every packet to fail parsing.
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

    // CHANGED: Alert fields are now inline to match the OCS wire format exactly.
    // OCS sends misses/count/fault_type as direct fields, not nested in a struct.
    Alert
    {
        event: String,
        misses: Option<u32>,        // how many thermal readings were missed in a row
        count: Option<u32>,         // how many times this fault has occurred
        fault_type: Option<String>, // what kind of fault, e.g. "CorruptedReading"
    },
}

// The command we send back to the OCS over UDP.
// Only includes fields the OCS expects to see.
#[derive(Serialize, Deserialize, Debug)]
struct GCSCommand 
{
    pub tag: String,  // always "command" so OCS knows how to parse it
    pub command: String,  // e.g. "ThermalCheck" or "EmergencyHalt"
    pub timestamp: u64,     // simulation-relative time in ms when this command was created
}

// Serialise a GCSCommand to a JSON string so it can be sent over UDP
fn encode_command(command: &GCSCommand) -> String
{
    serde_json::to_string(command)
        .unwrap_or_else(|error|
        {
            println!("[ENCODE] {error}");
            String::new()
        })
}


// ==============
// Typestate pattern 
// ==============

// The two possible GCS operating modes
struct Normal;

// Generic GCS mode struct — S carries the current mode as a type marker
struct GCSMode<S>
{
    state: PhantomData<S>  // zero cost, no memory at runtime
}

// Methods available ONLY when in Normal mode
impl GCSMode<Normal>
{
    fn new() -> Self
    {
        GCSMode { state: PhantomData }
    }
}

// GCSMode<Normal> to call this function, which can only be constructed after
// confirming fault_active is false at runtime.
fn dispatch_command(normal_mode: &GCSMode<Normal>, command_sender: &mpsc::Sender<UplinkCommand>, payload: String)
{
    let _ = normal_mode;  // used only for the compile-time type check — no runtime cost
    // try_send is non-blocking — if the channel is full we silently drop the command
    let _ = command_sender.try_send(UplinkCommand
    {
        payload,
        created_at: Instant::now(),
    });
}

// ===============
// Data Types
// ===============
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CompressedReading
{
    sensor_type: String,
    sequence: u64,
    value: f64,
    drift: i64,
    timestamp: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct CompressedPayload
{
    packet_id: u64,
    reading_count: usize,
    created_at: u64,
    readings: Vec<CompressedReading>,
}

// Represents one raw packet we just received from the OCS
#[derive(Debug)]
struct IncomingPacket
{
    payload: String,
    received_at: Instant,   // stamped right when the packet arrives, before any processing
}

// Represents one command waiting in the queue to be sent to the OCS
#[derive(Debug, Clone)]
struct UplinkCommand
{
    payload:    String,
    created_at: Instant,  // used to measure how long the command waited before being sent
}

// Used to categorise incoming OCS alert events into named types
#[derive(Debug)]
enum OCSFaultKind
{
    ThermalAlert,
    FaultInjected,
    MissionAbort,
    Unknown,
}

// All the performance metrics we collect during the simulation
#[derive(Default)]
struct GCSMetrics
{
    // Telemetry reception stats
    telemetry_received: u64,
    missed_packets: u64,

    // Latency measurements
    decode_latency: Vec<u64>,   // how long each packet took to decode (µs)
    reception_drift: Vec<i64>,   // gap between arriving packets vs expected timing

    // Command uplink stats
    commands_sent: u64,
    commands_rejected: u64,
    dispatch_latency: Vec<u64>,  // how long each command waited before being sent
    rejection_log: Vec<String>,

    // Deadline violations
    deadline_violations: Vec<String>,

    // Jitter per RM task — variation in how consistently each period fires
    thermal_jitter: Vec<i64>,
    accelerometer_jitter: Vec<i64>,
    gyroscope_jitter: Vec<i64>,

    // Fault handling
    faults_received: u64,
    interlock_latency: Vec<u64>,   // time from fault detection to command block
    critical_alerts: Vec<String>,

    // CPU and scheduling drift
    drift: Vec<i64>,  // difference between expected vs actual task start times
    active_time: u64,       // total µs the tasks were actually doing work
    elapsed_time: u64,       // total ms the simulation has been running

    // Backlog tracking
    backlog_depth_samples: Vec<usize>,  // snapshot of channel depth every 10s
    backlog_peak: usize,       // highest backlog we saw during the run
}

// The shared runtime state of the GCS, written and read from multiple tasks
struct GCSState
{
    fault_active: bool,
    fault_detected_at: Option<Instant>,
    fault_log: Vec<String>,

    // Miss counters for each sensor type — increment when no packet arrives in time
    thermal_misses: u32,
    accelerometer_misses: u32,
    gyroscope_misses: u32,

    // Last time we received data from each sensor (in simulation ms)
    last_thermal: u64,
    last_accelerometer: u64,
    last_gyroscope: u64,

    loss_of_contact: bool,
}

// Default state — everything starts at 0 / false / empty
impl Default for GCSState
{
    fn default() -> Self
    {
        GCSState
        {
            fault_active: false,
            fault_detected_at: None,
            fault_log: Vec::new(),
            thermal_misses: 0,
            accelerometer_misses: 0,
            gyroscope_misses: 0,
            last_thermal: 0,
            last_accelerometer: 0,
            last_gyroscope: 0,
            loss_of_contact: false,
        }
    }
}

// ===============
// Helper functions
// ===============

// Type alias to make Arc<Mutex<T>> less verbose in function signatures
type Shared<T> = Arc<Mutex<T>>;

// How many milliseconds have passed since the simulation started
fn simulation_elapsed(simulation_start: &Instant) -> u64
{
    simulation_start.elapsed().as_millis() as u64
}

// Write a line to the log file only (doesn't print to terminal)
fn write_log(logger: &Shared<File>, line: &str)
{
    let mut log_file = logger.lock().unwrap();
    let _ = writeln!(log_file, "{line}");
}

// Print to the terminal AND write to the log file — used for important events
fn print_and_log(logger: &Shared<File>, line: &str)
{
    println!("{line}");
    write_log(logger, line);
}

// Print one row of the final stats table: label, count, min, max, average
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

    println!("    {:<36} n={:>5}  min={:>7}  max={:>7}  avg={:>9.1}", label, samples.len(), minimum_value, maximum_value, average_value);
}

// ===============
// Log file setup
// ===============
fn create_logger() -> Shared<File>
{
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)  // start fresh each run
        .open(LOG_FILE)
        .expect("[LOG] Failed to create log file");

    Arc::new(Mutex::new(log_file))
}

// ===============
// Task 1: UDP Receiver Loop
// Listens for incoming telemetry from the OCS and forwards it to the processor
// ===============
async fn udp_receiver_loop(socket: Arc<UdpSocket>, incoming_sender: mpsc::Sender<IncomingPacket>,
backlog_counter: Shared<usize>, logger: Shared<File>,
simulation_start: Instant, shutdown: CancellationToken)
{
    write_log(&logger, &format!("[{}ms] [UDP Receiver] Listening for OCS telemetry on {GCS_TELEMETRY_BIND}", simulation_elapsed(&simulation_start)));

    let mut buffer = vec![0u8; 4096];  // 4096 bytes is more than enough for a JSON telemetry packet

    loop
    {
        // Wait for either a shutdown signal or an incoming packet, whichever comes first
        tokio::select!
        {
            // Shutdown was requested — exit cleanly
            _ = shutdown.cancelled() =>
            {
                write_log(&logger, &format!("[{}ms] [UDP Receiver] Exit.", simulation_elapsed(&simulation_start)));
                return;
            }

            // A packet arrived on the socket
            result = socket.recv_from(&mut buffer) =>
            {
                match result
                {
                    Ok((len, sender_address)) => // CHANGED: was 'from' which is a reserved-feeling short name — renamed to sender_address
                    {
                        // Stamp the arrival time right away, before any processing
                        let received_at = Instant::now();
                        let payload = String::from_utf8_lossy(&buffer[..len]).to_string();

                        // Forward to the telemetry processor via the channel
                        if incoming_sender.try_send(IncomingPacket {payload, received_at}).is_ok()
                        {
                            // Keep the backlog counter in sync
                            *backlog_counter.lock().unwrap() += 1;
                        }
                        else
                        {
                            // Channel is full — we had to drop this packet
                            print_and_log(&logger, &format!("[{}ms] [UDP Receiver] Channel full — packet dropped from {sender_address}", simulation_elapsed(&simulation_start))); // CHANGED: updated to use sender_address
                        }
                    }
                    Err(error) => print_and_log(&logger, &format!("[{}ms] [UDP Receiver] recv_from error: {error}", simulation_elapsed(&simulation_start)))
                }
            }
        }
    }
}

// ===============
// Task 2: UDP Sender Task
// Takes commands from the queue and actually sends them to the OCS over UDP
// ===============
async fn udp_sender_task(mut receiver: mpsc::Receiver<UplinkCommand>,
socket: Arc<UdpSocket>, metrics: Shared<GCSMetrics>,
logger: Shared<File>, simulation_start: Instant,
shutdown: CancellationToken)
{
    write_log(&logger, &format!("[{}ms] [UDP Sender] Command link established -> {OCS_COMMAND_ADDRESS}", simulation_elapsed(&simulation_start)));

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                write_log(&logger, &format!("[{}ms] [UDP Sender] Exit.", simulation_elapsed(&simulation_start)));
                return;
            }

            command = receiver.recv() =>
            {
                match command
                {
                    Some(command) =>
                    {
                        // Measure how long the command waited in the queue before being sent
                        let dispatch = command.created_at.elapsed().as_micros() as u64;
                        let dispatch_ms = dispatch / 1000;

                        match socket.send_to(command.payload.as_bytes(), OCS_COMMAND_ADDRESS).await
                        {
                            Ok(_) =>
                            {
                                // Check if we missed the dispatch deadline
                                if dispatch_ms > DISPATCH_DEADLINE
                                {
                                    let violation = format!("[{}ms] [DEADLINE] Dispatch {}ms > {}ms", simulation_elapsed(&simulation_start), dispatch_ms, DISPATCH_DEADLINE);
                                    
                                    print_and_log(&logger, &violation);
                                    metrics.lock().unwrap().deadline_violations.push(violation);
                                }
                                else
                                {
                                    write_log(&logger, &format!("[{}ms] [UDP Sender] Sent: {}", simulation_elapsed(&simulation_start), command.payload));
                                }

                                let mut metric = metrics.lock().unwrap();
                                metric.commands_sent += 1;
                                metric.dispatch_latency.push(dispatch);
                            }
                            Err(error) => print_and_log(&logger, &format!("[{}ms] [UDP Sender] send_to error: {error}", simulation_elapsed(&simulation_start))),
                        }
                    }
                    None => return,  // channel was closed, time to exit
                }
            }
        }
    }
}

// ===============
// Part 1 — Telemetry Processor
// Decodes incoming packets and routes them to the correct state update
// ===============

// This is the synchronous decode function — called inside spawn_blocking
// so it doesn't hold up the async runtime while parsing JSON
fn decode_packet_sync(payload: String) -> Result<OCSMessage, String>
{
    serde_json::from_str::<OCSMessage>(&payload)
        .map_err(|error| format!("serde parse error: {error}  payload={payload}"))
}

async fn telemetry_processor_task(mut receiver: mpsc::Receiver<IncomingPacket>, state: Shared<GCSState>, metrics: Shared<GCSMetrics>, command_sender: mpsc::Sender<UplinkCommand>, backlog_counter: Shared<usize>, logger: Shared<File>, simulation_start: Instant, shutdown: CancellationToken)
{
    write_log(&logger, &format!("[{}ms] [Telemetry Receiver] Ready.  decode_deadline={}ms", simulation_elapsed(&simulation_start), DECODE_DEADLINE));
 
    // Track the last time we received a packet so we can calculate reception drift
    let mut last_received_at: Option<Instant> = None;
 
    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                write_log(&logger, &format!("[{}ms] [Telemetry Receiver] Exit.", simulation_elapsed(&simulation_start)));
                return;
            }
 
            packet = receiver.recv() =>
            {
                let packet = match packet
                {
                    None    => return,  // channel closed — we're done
                    Some(packet) => packet,
                };
 
                // One packet consumed, so decrement the backlog counter
                {
                    let mut depth = backlog_counter.lock().unwrap();
                    if *depth > 0 
                    { 
                        *depth -= 1; 
                    }
                }
 
                // Save the arrival time before we do anything else
                let arrival_time = packet.received_at;
                let payload = packet.payload.clone();
 
                // Offload the JSON parsing to a blocking thread so we don't delay other async tasks
                let parse_result = tokio::task::spawn_blocking(move || decode_packet_sync(payload)).await;
 
                // Measure decode time in microseconds for better precision
                let decode_time = arrival_time.elapsed().as_micros() as u64;
                let decode_time_ms = decode_time / 1000;
 
                // Check if we missed the decode deadline
                if decode_time_ms > DECODE_DEADLINE
                {
                    let violation = format!("[{}ms] [DEADLINE] Decode {}ms > {}ms", simulation_elapsed(&simulation_start), decode_time_ms, DECODE_DEADLINE);
                
                    print_and_log(&logger, &violation);
                    metrics.lock().unwrap().deadline_violations.push(violation);
                }
 
                // Unwrap the JoinHandle result, then the parse result
                let message = match parse_result
                {
                    Err(_) => {write_log(&logger, "[Telemetry Receiver] spawn_blocking panic"); continue; }
                    Ok(Err(why)) => {write_log(&logger, &format!("[Telemetry Receiver] Parse fail: {why}")); continue; }
                    Ok(Ok(m)) => m,
                };
 
                // Calculate the gap since the last packet arrived (inter-packet drift)
                let inter_packet_gap: i64 = match last_received_at
                {
                    Some(time) => time.elapsed().as_millis() as i64,
                    None => 0,
                };
                last_received_at = Some(Instant::now());
 
                // Route each message type to the right handler
                // Route each message type to the right handler
                match &message
                {
                    // =============
                    // Alert Message
                    // =============
                    OCSMessage::Alert {..} =>
                    {
                        handle_ocs_alert(&message, &state, &metrics, &command_sender, &logger, &simulation_start);
                        continue;  // alerts don't count toward normal telemetry metrics
                    }

                    // =============
                    // Status Message
                    // Receiving Status means the satellite is alive —
                    // reset the thermal watchdog so it doesn't false-fire.
                    // =============
                    OCSMessage::Status {iteration, fill, state: system_state, drift} =>
                    {
                        write_log(&logger, &format!(
                            "[{}ms] [Telemetry Receiver] status  iteration={iteration}  fill={fill:.1}%  state={}  drift={}ms  decode={}µs",
                            simulation_elapsed(&simulation_start), system_state, drift, decode_time
                        ));

                        let mut gcs_state = state.lock().unwrap();

                        gcs_state.thermal_misses = 0;
                        gcs_state.accelerometer_misses = 0;
                        gcs_state.gyroscope_misses = 0;
                        gcs_state.last_thermal = simulation_elapsed(&simulation_start);
                        gcs_state.last_accelerometer = simulation_elapsed(&simulation_start);
                        gcs_state.last_gyroscope = simulation_elapsed(&simulation_start);

                        if gcs_state.loss_of_contact
                        {
                            gcs_state.loss_of_contact = false;
                        }
                    }

                    // =============
                    // Downlink Message
                    // The OCS bundles all sensor readings into one compressed packet.
                    // We unpack each reading here and update state just like the old
                    // individual Thermal/Accelerometer/Gyroscope messages used to.
                    // =============
                    OCSMessage::Downlink { packet_id, reading_count, bytes, queue_latency, payload } =>
                    {
                        // Log the outer packet summary — 6 placeholders, 6 args after timestamp
                        write_log(&logger, &format!("[{}ms] [Telemetry Receiver] downlink packet_id={}  reading_count={}  {}bytes  queue_latency={}ms  decode={}µs", simulation_elapsed(&simulation_start), packet_id, reading_count, bytes, queue_latency, decode_time));

                        let compressed_payload = match serde_json::from_str::<CompressedPayload>(payload)
                        {
                            Ok(parsed_payload) => parsed_payload,
                            Err(error) =>
                            {
                                write_log(&logger, &format!("[{}ms] [Telemetry Receiver] downlink inner payload parse fail: {}", simulation_elapsed(&simulation_start), error));
                                continue;
                            }
                        };

                        let mut gcs_state = state.lock().unwrap();

                        // Walk every reading in the bundle and route it to the right state update.
                        for reading in &compressed_payload.readings
                        {
                            match reading.sensor_type.as_str()
                            {
                                "Thermal" =>
                                {
                                    write_log(&logger, &format!("[{}ms] [Telemetry Receiver] thermal  seq={}  temp={:.2}C  drift={:+}ms", simulation_elapsed(&simulation_start), reading.sequence, reading.value, reading.drift));
                               
                                    gcs_state.thermal_misses = 0;
                                    gcs_state.last_thermal = simulation_elapsed(&simulation_start);
                                }

                                "Accelerometer" =>
                                {
                                    write_log(&logger, &format!("[{}ms] [Telemetry Receiver] accelerometer  seq={}  velocity={:.4}  drift={:+}ms", simulation_elapsed(&simulation_start), reading.sequence, reading.value, reading.drift));
                                
                                    gcs_state.accelerometer_misses = 0;
                                    gcs_state.last_accelerometer = simulation_elapsed(&simulation_start);
                                }

                                "Gyroscope" =>
                                {
                                    write_log(&logger, &format!("[{}ms] [Telemetry Receiver] gyroscope  seq={}  orientation={:.4}  drift={:+}ms", simulation_elapsed(&simulation_start), reading.sequence, reading.value, reading.drift));
                           
                                    gcs_state.gyroscope_misses = 0;
                                    gcs_state.last_gyroscope = simulation_elapsed(&simulation_start);
                                }

                                other =>
                                {
                                    write_log(&logger, &format!("[{}ms] [Telemetry Receiver] unknown sensor_type in downlink: {}", simulation_elapsed(&simulation_start), other));
                                }
                            }
                        }

                        // Any downlink packet proves the satellite is alive
                        if gcs_state.loss_of_contact
                        {
                            gcs_state.loss_of_contact = false;
                        }
                    }

                } 

                // Update metrics for every successfully decoded packet
                let mut metric = metrics.lock().unwrap();
                metric.telemetry_received += 1;
                metric.decode_latency.push(decode_time);
                metric.reception_drift.push(inter_packet_gap);
            }
        }
    }
}


// Handles an incoming OCS Alert message.
// Checks the alert type and decides whether to engage the safety interlock.
fn handle_ocs_alert(message: &OCSMessage, state: &Shared<GCSState>, metrics: &Shared<GCSMetrics>, command_sender: &mpsc::Sender<UplinkCommand>, logger: &Shared<File>,simulation_start: &Instant)
{
    // Pull the event string out of the Alert variant
    let event = match message
    {
        OCSMessage::Alert {event, ..} => event.as_str(), // CHANGED: removed TODO comment
        _ => return,
    };

    // Convert the event string to our typed enum so we can match on it
    let fault_kind = match event
    {
        "mission_abort" => OCSFaultKind::MissionAbort,
        "thermal_alert" => OCSFaultKind::ThermalAlert,
        "fault" => OCSFaultKind::FaultInjected,
        _ => OCSFaultKind::Unknown,
    };

    let fault_message = format!("[{}ms] [OCS-ALERT] {fault_kind:?}  event=\"{event}\"", simulation_elapsed(simulation_start));
    print_and_log(logger, &fault_message);

    // Record the fault — release lock before the next one
    metrics.lock().unwrap().faults_received += 1;
    state.lock().unwrap().fault_log.push(fault_message);

    match fault_kind
    {
        OCSFaultKind::MissionAbort =>
        {
            // This is the most serious — log it as critical and send EmergencyHalt immediately
            let alert = format!("[{}ms] !! CRITICAL GROUND ALERT !! Mission Abort", simulation_elapsed(simulation_start));
            print_and_log(logger, &alert);
            metrics.lock().unwrap().critical_alerts.push(alert);

            {
                let mut state = state.lock().unwrap();
                if !state.fault_active
                {
                    state.fault_active = true;
                    state.fault_detected_at = Some(Instant::now());
                }
            }

            // EmergencyHalt bypasses normal typestate checking — it's urgent
            let payload = encode_command(&GCSCommand
            {
                tag: "command".into(),
                command: "EmergencyHalt".into(),
                timestamp: simulation_elapsed(simulation_start),
            });
            let _ = command_sender.try_send(UplinkCommand
            {
                payload,
                created_at: Instant::now(),
            });
        }

        OCSFaultKind::ThermalAlert | OCSFaultKind::FaultInjected  => 
        {
            // Engage the safety interlock — block all commands until the fault clears
            let mut state = state.lock().unwrap();
            if !state.fault_active
            {
                state.fault_active = true;
                state.fault_detected_at = Some(Instant::now());
                print_and_log(logger, &format!("[{}ms] [Fault Manager] Safety interlock ENGAGED — commands blocked", simulation_elapsed(simulation_start)));
            }
        }

        OCSFaultKind::Unknown =>
        {
            // Log it but don't engage the interlock for unknown events
            print_and_log(logger, &format!("[{}ms] [Fault Manager] Unknown alert \"{event}\" — logged, interlock not engaged", simulation_elapsed(simulation_start)));
        }
    }
}

// ===============
// Task 3: Loss-of-Contact Monitor
// Periodically checks if any sensor has gone silent for too long
// ===============
async fn loss_of_contact_monitor_task(state: Shared<GCSState>, metrics: Shared<GCSMetrics>, command_sender: mpsc::Sender<UplinkCommand>, logger: Shared<File>, simulation_start: Instant, shutdown: CancellationToken,)
{
    write_log(&logger, &format!("[{}ms] [Loss of Contact Monitor] Started. threshold={LOSS_OF_CONTACT_MISS_THRESHOLD}", simulation_elapsed(&simulation_start)));

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                write_log(&logger, &format!("[{}ms] [Loss of Contact Monitor] Exit.", simulation_elapsed(&simulation_start)));
                return;
            }

            _ = sleep(Duration::from_millis(REREQUEST_INTERVAL)) => {}
        }

        let current_time = simulation_elapsed(&simulation_start);

        // ===============================
        // Step 1: Update miss counters
        // ===============================
        let (thermal_misses, accelerometer_misses, gyroscope_misses, loss_of_contact_flag);

        {
            let mut gcs_state = state.lock().unwrap();

            // Increment miss counter only if that sensor has been quiet longer than expected
            if current_time.saturating_sub(gcs_state.last_thermal) > THERMAL_COMMAND_PERIOD * 6
            {
                gcs_state.thermal_misses += 1;
            }

            if current_time.saturating_sub(gcs_state.last_accelerometer) > ACCELEROMETER_COMMAND_PERIOD * 6
            {
                gcs_state.accelerometer_misses += 1;
            }

            if current_time.saturating_sub(gcs_state.last_gyroscope) > GYROSCOPE_COMMAND_PERIOD * 4
            {
                gcs_state.gyroscope_misses += 1;
            }

            // Copy values out so we don't hold the lock any longer than needed
            thermal_misses = gcs_state.thermal_misses;
            accelerometer_misses = gcs_state.accelerometer_misses;
            gyroscope_misses = gcs_state.gyroscope_misses;
            loss_of_contact_flag = gcs_state.loss_of_contact;
        }

        // Find which sensor has missed the most packets
        let max_miss_count = thermal_misses.max(accelerometer_misses).max(gyroscope_misses);

        // ===============================
        // Step 2: Detect Loss of Contact
        // ===============================
        if max_miss_count >= LOSS_OF_CONTACT_MISS_THRESHOLD && !loss_of_contact_flag
        {
            let alert_message = format!("[{}ms] !! LOSS OF CONTACT !! thermal={} accelerometer={} gyroscope={}", current_time, thermal_misses, accelerometer_misses, gyroscope_misses
            );
            write_log(&logger, &alert_message);
            {
                let mut metric = metrics.lock().unwrap();
                metric.critical_alerts.push(alert_message);
                metric.missed_packets += 1;
            }

            {
                let mut state = state.lock().unwrap();
                state.loss_of_contact = true;
            }

            // Ask the OCS to resend the missing data
            let payload = encode_command(&GCSCommand
            {
                tag: "command".into(),
                command: "RetransmitRequest".into(),
                timestamp: current_time,
            });

            let _ = command_sender.try_send(UplinkCommand
            {
                payload,
                created_at: Instant::now(),
            });

            write_log(
                &logger,
                &format!("[{}ms] [Loss of Contact Monitor] Re-request sent to OCS.", current_time),
            );
        }

        // ===============================
        // Step 3: Contact Restored
        // ===============================
        else if loss_of_contact_flag && max_miss_count < LOSS_OF_CONTACT_MISS_THRESHOLD
        {
            // We're receiving again — reset everything
            {
                let mut state = state.lock().unwrap();
                state.loss_of_contact = false;
                state.thermal_misses = 0;
                state.accelerometer_misses = 0;
                state.gyroscope_misses = 0;
            }

            write_log(&logger,&format!("[{}ms] [Loss of Contact Monitor] Contact restored — counters reset.",current_time));
        }
    }
}

// ===============
// Task 4: Fault Manager
// Monitors active faults and checks if the interlock response time exceeds the limit
// ===============
async fn fault_manager_task(state: Shared<GCSState>, metrics: Shared<GCSMetrics>, logger: Shared<File>, simulation_start: Instant, shutdown: CancellationToken,)
{
    write_log(&logger, &format!("[{}ms] [Fault Manager] Started.  response_limit={}ms", simulation_elapsed(&simulation_start),FAULT_RESPONSE_LIMIT));

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                write_log(&logger, &format!("[{}ms] [Fault Manager] Exit.", simulation_elapsed(&simulation_start)));
                return;
            }
            _ = sleep(Duration::from_millis(50)) => {}
        }

        // Nothing to do if there's no active fault
        if !state.lock().unwrap().fault_active {continue;}

        // Get the time the fault was first detected — lock released immediately after
        let fault_detected_at = {
            state.lock().unwrap().fault_detected_at
        };

        if let Some(detection_time) = fault_detected_at
        {
            let interlock_time = detection_time.elapsed().as_millis() as u64;

            metrics.lock().unwrap().interlock_latency.push(interlock_time);

            // If the interlock took too long, log a critical alert and auto-clear
            if interlock_time > FAULT_RESPONSE_LIMIT
            {
                let alert = format!(
                    "[{}ms] !! CRITICAL ALERT !! Interlock {interlock_time}ms > {FAULT_RESPONSE_LIMIT}ms",
                    simulation_elapsed(&simulation_start)
                );
                print_and_log(&logger, &alert);
                metrics.lock().unwrap().critical_alerts.push(alert);

                {
                    let mut state = state.lock().unwrap();
                    state.fault_active = false;
                    state.fault_detected_at = None;
                }

                write_log(&logger, &format!("[{}ms] [Fault Manager] Interlock auto-cleared.", simulation_elapsed(&simulation_start)));
            }
        }
    }
}

// ===============
// Part 2 — Command Uplink Tasks
// Three Rate Monotonic tasks that periodically send check commands to the OCS.
// RM scheduling: shortest period = highest priority.
// The GCS command periods are 2x the OCS sensor periods —
// fast enough to monitor health without flooding the uplink.
// ===============

// Thermal Check — RM Priority 1 (fastest period = highest priority)
async fn thermal_command_task(command_sender: mpsc::Sender<UplinkCommand>, state: Shared<GCSState>, metrics: Shared<GCSMetrics>, logger: Shared<File>, simulation_start: Instant, shutdown: CancellationToken)
{
    write_log(&logger, &format!("[{}ms] [Thermal Command] Started.  period={}ms  RM-P1", simulation_elapsed(&simulation_start), THERMAL_COMMAND_PERIOD));

    let mut iteration: u64 = 0;
    let task_start: Instant = Instant::now();    // reference point for drift calculation
    let mut last_tick: Instant = Instant::now(); // reference point for jitter calculation

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                write_log(&logger, &format!("[{}ms] [Thermal Command] Exit.", simulation_elapsed(&simulation_start)));
                return;
            }
            _ = sleep(Duration::from_millis(THERMAL_COMMAND_PERIOD)) => {}
        }
        let loop_start = Instant::now();

        // Drift: actual elapsed vs expected elapsed — positive means running late
        let expected_time = iteration * THERMAL_COMMAND_PERIOD;
        let actual_time   = task_start.elapsed().as_millis() as u64;
        let drift         = actual_time as i64 - expected_time as i64;

        // Jitter: deviation of this interval from the ideal period (in microseconds)
        let this_interval = last_tick.elapsed().as_micros() as i64;
        let jitter_timing = (this_interval - (THERMAL_COMMAND_PERIOD as i64 * 1000)).unsigned_abs() as i64;
        last_tick = Instant::now();

        // Safety interlock — block all commands while a fault is active
        if state.lock().unwrap().fault_active
        {
            let rejection_message = format!("[{}ms] [REJECT] ThermalCheck iteration={iteration} — FaultLocked", simulation_elapsed(&simulation_start));
            write_log(&logger, &rejection_message);
            let mut metric = metrics.lock().unwrap();
            metric.commands_rejected += 1;
            metric.rejection_log.push(rejection_message);
            iteration += 1;
            continue;
        }

        // Typestate guard — can only construct GCSMode<Normal> when fault_active is false.
        // dispatch_command requires &GCSMode<Normal>, so this is compile-time proof
        // that the interlock was checked before dispatching.
        let normal_mode = GCSMode::<Normal>::new();

        let payload = encode_command(&GCSCommand
        {
            tag:       "command".into(),
            command:   "ThermalCheck".into(),
            timestamp: simulation_elapsed(&simulation_start),
        });

        dispatch_command(&normal_mode, &command_sender, payload); // RM-P1

        // Jitter warning — log as deadline violation if over the 2ms uplink limit
        if jitter_timing > UPLINK_JITTER_LIMIT
        {
            let warning = format!("[{}ms] [WARN] Thermal Command jitter {}µs iteration={iteration}", simulation_elapsed(&simulation_start), jitter_timing);
            write_log(&logger, &warning);
        }

        {
            let mut metric = metrics.lock().unwrap();
            metric.thermal_jitter.push(jitter_timing);
            metric.drift.push(drift);
            metric.active_time  += loop_start.elapsed().as_micros() as u64;
            metric.elapsed_time  = task_start.elapsed().as_millis() as u64; // kept here — Thermal runs most often so it tracks sim time accurately
        }

        // Log every 20 iterations — 50ms period means every ~1 second
        if iteration % 20 == 0
        {
            write_log(&logger, &format!("[{}ms] [Thermal Command] iteration={iteration:>4}  drift={drift:+}ms  jitter={jitter_timing}µs", simulation_elapsed(&simulation_start)));
        }
        iteration += 1;
    }
}

// Accelerometer Check — RM Priority 2
async fn accelerometer_command_task(command_sender: mpsc::Sender<UplinkCommand>, state: Shared<GCSState>, metrics: Shared<GCSMetrics>, logger: Shared<File>, simulation_start: Instant, shutdown: CancellationToken)
{
    write_log(&logger, &format!("[{}ms] [Accelerometer Command] Started.  period={}ms  RM-P2", simulation_elapsed(&simulation_start), ACCELEROMETER_COMMAND_PERIOD));

    let mut iteration: u64 = 0;
    let task_start: Instant = Instant::now();
    let mut last_tick: Instant = Instant::now();

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                write_log(&logger, &format!("[{}ms] [Accelerometer Command] Exit.", simulation_elapsed(&simulation_start)));
                return;
            }
            _ = sleep(Duration::from_millis(ACCELEROMETER_COMMAND_PERIOD)) => {}
        }
        let loop_start = Instant::now();

        // Drift: actual elapsed vs expected elapsed
        let expected_time = iteration * ACCELEROMETER_COMMAND_PERIOD;
        let actual_time   = task_start.elapsed().as_millis() as u64;
        let drift         = actual_time as i64 - expected_time as i64;

        // Jitter: microseconds precision — 120ms period makes ms too coarse
        let this_interval = last_tick.elapsed().as_micros() as i64;
        let jitter_timing = (this_interval - (ACCELEROMETER_COMMAND_PERIOD as i64 * 1000)).unsigned_abs() as i64;
        last_tick = Instant::now();

        // Safety interlock — block all commands while a fault is active
        if state.lock().unwrap().fault_active
        {
            let rejection_message = format!("[{}ms] [REJECT] AccelerometerCheck iteration={iteration} — FaultLocked", simulation_elapsed(&simulation_start));
            write_log(&logger, &rejection_message);
            let mut metric = metrics.lock().unwrap();
            metric.commands_rejected += 1;
            metric.rejection_log.push(rejection_message);
            iteration += 1;
            continue;
        }

        let normal_mode = GCSMode::<Normal>::new();

        let payload = encode_command(&GCSCommand
        {
            tag:       "command".into(),
            command:   "AccelerometerCheck".into(),
            timestamp: simulation_elapsed(&simulation_start),
        });

        dispatch_command(&normal_mode, &command_sender, payload); // RM-P2

        // FIX: jitter warning was missing from Accelerometer — added to match Thermal and Gyroscope.
        // Without this, any jitter spike here is silently dropped and never reaches the final benchmarking report.
        if jitter_timing > UPLINK_JITTER_LIMIT
        {
            let warning = format!("[{}ms] [WARN] Accelerometer Command jitter {}µs iteration={iteration}", simulation_elapsed(&simulation_start), jitter_timing);
            write_log(&logger, &warning);
        }

        {
            let mut metric = metrics.lock().unwrap();
            metric.accelerometer_jitter.push(jitter_timing);
            metric.drift.push(drift);
            metric.active_time += loop_start.elapsed().as_micros() as u64;
        }

        // Log every 8 iterations — 120ms period means every ~1 second
        if iteration % 8 == 0
        {
            write_log(&logger, &format!("[{}ms] [Accelerometer Command] iteration={iteration:>4}  drift={drift:+}ms  jitter={jitter_timing}µs", simulation_elapsed(&simulation_start)));
        }

        iteration += 1;
    }
}

// Gyroscope Check — RM Priority 3 (slowest period = lowest priority)
async fn gyroscope_command_task(command_sender: mpsc::Sender<UplinkCommand>, state: Shared<GCSState>, metrics: Shared<GCSMetrics>, logger: Shared<File>, simulation_start: Instant, shutdown: CancellationToken)
{
    write_log(&logger, &format!("[{}ms] [Gyroscope Command] Started.  period={}ms  RM-P3", simulation_elapsed(&simulation_start), GYROSCOPE_COMMAND_PERIOD));

    let mut iteration: u64 = 0;
    let task_start: Instant = Instant::now();
    let mut last_tick: Instant = Instant::now();

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() =>
            {
                write_log(&logger, &format!("[{}ms] [Gyroscope Command] Exit.", simulation_elapsed(&simulation_start)));
                return;
            }
            _ = sleep(Duration::from_millis(GYROSCOPE_COMMAND_PERIOD)) => {}
        }
        let loop_start = Instant::now();

        // Drift and jitter — same pattern as the other two RM tasks
        let expected_time = iteration * GYROSCOPE_COMMAND_PERIOD;
        let actual_time   = task_start.elapsed().as_millis() as u64;
        let drift         = actual_time as i64 - expected_time as i64;

        let this_interval = last_tick.elapsed().as_micros() as i64;
        let jitter_timing = (this_interval - (GYROSCOPE_COMMAND_PERIOD as i64 * 1000)).unsigned_abs() as i64;
        last_tick = Instant::now();

        // Safety interlock — block all commands while a fault is active
        if state.lock().unwrap().fault_active
        {
            let rejection_message = format!("[{}ms] [REJECT] GyroscopeCheck iteration={iteration} — FaultLocked", simulation_elapsed(&simulation_start));
            write_log(&logger, &rejection_message);
            let mut metric = metrics.lock().unwrap();
            metric.commands_rejected += 1;
            metric.rejection_log.push(rejection_message);
            iteration += 1;
            continue;
        }

        let normal_mode = GCSMode::<Normal>::new();

        let payload = encode_command(&GCSCommand
        {
            tag:       "command".into(),
            command:   "GyroscopeCheck".into(),
            timestamp: simulation_elapsed(&simulation_start),
        });

        dispatch_command(&normal_mode, &command_sender, payload); // RM-P3

        // Jitter warning — same threshold as Thermal and Accelerometer
        if jitter_timing > UPLINK_JITTER_LIMIT
        {
            let warning = format!("[{}ms] [WARN] Gyroscope Command jitter {}µs iteration={iteration}", simulation_elapsed(&simulation_start), jitter_timing);
            write_log(&logger, &warning);
        }

        {
            let mut metric = metrics.lock().unwrap();
            metric.gyroscope_jitter.push(jitter_timing);
            metric.drift.push(drift);
            metric.active_time += loop_start.elapsed().as_micros() as u64;
        }

        // Log every 5 iterations — 333ms period means every ~1.6 seconds
        if iteration % 5 == 0
        {
            write_log(&logger, &format!("[{}ms] [Gyroscope Command] iteration={iteration:>4}  drift={drift:+}ms  jitter={jitter_timing}µs", simulation_elapsed(&simulation_start)));
        }

        iteration += 1;
    }
}

// ===============
// Task 5: Metrics Reporter
// Snapshots the backlog depth every 10 seconds for the final benchmarking report
// ===============
async fn metrics_reporter_task(metrics: Shared<GCSMetrics>, _: Shared<GCSState>, backlog_counter: Shared<usize>, _: Shared<File>, _simulation_start: Instant, shutdown: CancellationToken)
{
    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() => return,
            _ = sleep(Duration::from_secs(10)) => {}
        }

        // Take a snapshot of the backlog every 10s so we can report average and peak at the end
        let current_depth = *backlog_counter.lock().unwrap();
        let mut metric = metrics.lock().unwrap();
        metric.backlog_depth_samples.push(current_depth);
        if current_depth > metric.backlog_peak {metric.backlog_peak = current_depth;}
    }
}

// ===============
// Final Benchmarking Report — printed at the end of the simulation
// ===============
fn print_report(metric: &GCSMetrics, state: &GCSState)
{
    // Calculate command rejection percentage
    let reject_percentage;
    if metric.commands_sent + metric.commands_rejected > 0
    {
        reject_percentage = metric.commands_rejected as f64 / (metric.commands_sent + metric.commands_rejected) as f64 * 100.0
    }
    else 
    {
        reject_percentage = 0.0
    }

    // Rough CPU estimate: how much of the elapsed time were tasks actually active?
    let cpu_estimate;
    if metric.elapsed_time > 0
    {
        cpu_estimate = metric.active_time as f64 / (metric.elapsed_time as f64 * 1000.0) * 100.0
    }
    else 
    {
        cpu_estimate = 0.0
    }


    println!("\n|==============================================================================================|");
    println!( "|               GCS FINAL BENCHMARKING REPORT - BY CHONG CHUN KIT (TP077436)                   |");
    println!( "|==============================================================================================|");
    println!(
        "|  Mode: {} ",
        if state.fault_active {"FaultLocked"} else {"Normal"}
    );
    println!(
        "|  Telemetry: receiver={}  missed={}",
        metric.telemetry_received, metric.missed_packets
    );
    println!(
        "|  Decode latency (µs)  deadline={}ms  (spawn_blocking)",
        DECODE_DEADLINE
    );
    let decode_samples: Vec<i64> = metric.decode_latency.iter().map(|&v| v as i64).collect();
    print_stat_row("Decode (µs)", &decode_samples);
    println!(
        "|  Commands  sent={}  rejected={} ({:.1}%)",
        metric.commands_sent, metric.commands_rejected, reject_percentage
    );
    println!(
        "|  Dispatch latency (µs)  deadline={}ms",
        DISPATCH_DEADLINE
    );
    let dispatch_samples: Vec<i64> = metric.dispatch_latency.iter().map(|&v| v as i64).collect();
    print_stat_row("Dispatch (µs)", &dispatch_samples);
    println!("|  Uplink jitter (µs)  limit={}µs", UPLINK_JITTER_LIMIT);
    print_stat_row("ThermalCheck       RM-P1", &metric.thermal_jitter);
    print_stat_row("AccelerometerCheck RM-P2", &metric.accelerometer_jitter);
    print_stat_row("GyroscopeCheck     RM-P3", &metric.gyroscope_jitter);
    println!("|  Drift (ms)");
    print_stat_row("All tasks", &metric.drift);
    println!(
        "|  Faults: {}  Critical alerts: {}",
        metric.faults_received, metric.critical_alerts.len()
    );
    if !metric.interlock_latency.is_empty()
    {
        let interlock_samples: Vec<i64> = metric.interlock_latency.iter().map(|&v| v as i64).collect();
        print_stat_row("Interlock Latency (ms)", &interlock_samples);
    }
    println!("|  Deadline violations: {}", metric.deadline_violations.len());
    for violation in metric.deadline_violations.iter().take(3)
    {
        println!("|    {violation}");
    }
    if metric.deadline_violations.len() > 3
    {
        println!("|    ... and {} more", metric.deadline_violations.len() - 3);
    }
        println!("|  Deadlines Met (%): {:.2}%", 100.0 - (metric.deadline_violations.len() as f64 / metric.drift.len() as f64  * 100.0));
    if !metric.rejection_log.is_empty()
    {
        println!("|  Rejections: {}", metric.rejection_log.len());
        for rejection in metric.rejection_log.iter().take(2)
        {
            println!("|    {rejection}");
        }
    }
    println!("|  CPU ≈ {cpu_estimate:.2}%");
    if !metric.critical_alerts.is_empty()
    {
        println!("|  CRITICAL ALERTS:");
        for alert in &metric.critical_alerts
        {
            println!("|    {alert}");
        }
    }
    println!("|==============================================================================================|\n");
}

// ===============
// Main Function — sets everything up and runs the simulation
// ===============
#[tokio::main]
async fn main()
{
    println!("|=======================================================================================================|");
    println!("|                         GROUND CONTROL STATION (GCS) — BY CHONG CHUN KIT (TP077436)                   |");
    println!("|                         TYPE: SOFT RTS, demonstrating the learnt Soft RTS concepts.                   |");
    println!("|=======================================================================================================|\n");

    // Record when the simulation started so we can measure elapsed time throughout
    let simulation_start = Instant::now();

    // Create the log file
    let logger = create_logger();
    println!("[GCS] log -> {LOG_FILE}");

    // Shared state — wrapped in Arc<Mutex<>> so multiple async tasks can access them safely
    let state   = Arc::new(Mutex::new(GCSState::default()));
    let metrics = Arc::new(Mutex::new(GCSMetrics::default()));

    // Backlog counter — tracks how many packets are waiting to be processed
    let backlog_counter: Shared<usize> = Arc::new(Mutex::new(0));

    // CancellationToken — when we call shutdown.cancel(), all tasks will stop cleanly
    let shutdown = CancellationToken::new();

    // Set up the UDP sockets
    let receiver_socket = Arc::new(UdpSocket::bind(GCS_TELEMETRY_BIND).await.expect("[GCS] bind receiver failed"));
    let send_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await.expect("[GCS] bind send failed"));

    // Two channels:
    // 1. incoming: from UDP receiver -> telemetry processor
    // 2. command: from RM tasks -> UDP sender
    let (incoming_sender, incoming_receiver) = mpsc::channel::<IncomingPacket>(100);
    let (command_sender, command_receiver) = mpsc::channel::<UplinkCommand>(50);

    println!("[GCS] Config:");
    println!("      Receive          : {GCS_TELEMETRY_BIND}  (serde_json decoded)");
    println!("      Send to          : {OCS_COMMAND_ADDRESS}  (serde_json encoded)");
    println!("      Typestate        : GCSMode<Normal/FaultLocked>  (PhantomData)");
    println!("      Decode           : spawn_blocking  (CPU-off-async)");
    println!(
        "      RM periods       : thermal={}ms  accelerometer={}ms  gyroscope={}ms",
        THERMAL_COMMAND_PERIOD, ACCELEROMETER_COMMAND_PERIOD, GYROSCOPE_COMMAND_PERIOD
    );
    println!("      Sim duration     : {SIMULATION_DURATION}s\n");

    // Spawn all the async tasks — each runs concurrently on the Tokio runtime

    // Listens for OCS telemetry and pushes it to the incoming channel
    tokio::spawn(udp_receiver_loop(Arc::clone(&receiver_socket), incoming_sender, Arc::clone(&backlog_counter), Arc::clone(&logger), simulation_start, shutdown.clone()));

    // Pulls commands from the queue and sends them to the OCS
    tokio::spawn(udp_sender_task(command_receiver, Arc::clone(&send_socket), Arc::clone(&metrics), Arc::clone(&logger), simulation_start, shutdown.clone()));

    // Decodes incoming packets and routes them to state updates
    tokio::spawn(telemetry_processor_task(incoming_receiver, Arc::clone(&state), Arc::clone(&metrics), command_sender.clone(), Arc::clone(&backlog_counter), Arc::clone(&logger), simulation_start, shutdown.clone()));

    // Watchdog — detects if any sensor goes silent for too long
    tokio::spawn(loss_of_contact_monitor_task(Arc::clone(&state), Arc::clone(&metrics), command_sender.clone(), Arc::clone(&logger), simulation_start, shutdown.clone()));

    // Checks if the safety interlock response time is within the 100ms limit
    tokio::spawn(fault_manager_task(Arc::clone(&state), Arc::clone(&metrics), Arc::clone(&logger), simulation_start, shutdown.clone()));

    // RM command tasks — fire at fixed periods, block if faulted
    tokio::spawn(thermal_command_task(command_sender.clone(), Arc::clone(&state), Arc::clone(&metrics), Arc::clone(&logger), simulation_start, shutdown.clone()));

    tokio::spawn(accelerometer_command_task(command_sender.clone(), Arc::clone(&state), Arc::clone(&metrics), Arc::clone(&logger), simulation_start, shutdown.clone() ));

    tokio::spawn(gyroscope_command_task(command_sender.clone(), Arc::clone(&state), Arc::clone(&metrics), Arc::clone(&logger), simulation_start, shutdown.clone()));

    // Snapshots backlog depth every 10 seconds
    tokio::spawn(metrics_reporter_task( Arc::clone(&metrics), Arc::clone(&state), Arc::clone(&backlog_counter), Arc::clone(&logger), simulation_start, shutdown.clone()));

    println!("[GCS] All tasks online.  Waiting for OCS telemetry...\n");

    // Let the simulation run for the full duration
    sleep(Duration::from_secs(SIMULATION_DURATION)).await;

    // Signal all tasks to stop
    let total_time = simulation_start.elapsed().as_millis();
    println!("\n[GCS] Simulation ended at {total_time}ms — shutting down...");
    shutdown.cancel();

    // Small delay to let tasks finish writing their final log lines
    sleep(Duration::from_millis(500)).await;

    // Print the final summary
    println!("\n[GCS] FINAL BENCHMARKING REPORT:");
    print_report(&metrics.lock().unwrap(), &state.lock().unwrap());
    println!("[GCS] Done.  Full event log -> {LOG_FILE}");
}