// SATELLITE ONBOARD CONTROL SYSTEM - BY LUVEN MARK (TP071542)
// SOFT RTS 

// =============================================================================
//  src/bin/OCS.rs   —   Satellite Onboard Control System (OCS)
//  CT087-3-3 Real-Time Systems  |  Student A
//
//  Run:   cargo run --bin OCS --release
//
//  Architecture: Soft Real-Time System on Tokio cooperative multitasking.
//  Lab 6 demonstrates exactly why this is Soft RTS:
//    "heavy_work().await grabs the thread and does NOT YIELD for 2 seconds"
//  Tokio cannot hard-preempt; tasks must voluntarily yield at every .await.
//  Therefore we track and log every deadline miss but cannot guarantee them.
//
//  Tutorial map — every major pattern traced to its source lab:
//  ─────────────────────────────────────────────────────────────
//  Lab 1  const, structs, enums, match, Vec, for, mut
//  Lab 2  Arc<Mutex<T>>, Arc::clone(), jitter formula with Instant
//  Lab 3  Instant::now() + .elapsed(), min/max/avg stat helper
//  Lab 6  #[tokio::main], tokio::spawn, sleep().await, heartbeat pattern
//  Lab 7  enum error variants + #[derive(Debug)], match arms,
//         consecutive-miss counter that resets on success
//  Lab 8  tokio::select!, CancellationToken, supervisor restart loop,
//         fragile task + match handle.await { Err(e) if e.is_panic() }
//  Lab 9  MqttOptions::new, AsyncClient::new, eventloop.poll() spawn,
//         client.publish(topic, QoS::AtMostOnce, false, payload).await
//  Lab 11 Arc<Mutex<Vec>> job queue, queue.lock().unwrap().pop(),
//         mpsc channel — tx.clone() / tx.send() / rx.recv()
// =============================================================================

// ── imports ───────────────────────────────────────────────────────────────────
use std::collections::BinaryHeap;         // lab 1 — data structures
use std::sync::{Arc, Mutex};              // lab 2 — Arc<Mutex<T>>
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};  // lab 3

use tokio::time::sleep;                   // lab 6 — non-blocking sleep
use tokio::sync::{mpsc, watch};          // lab 11 (mpsc), lab 8 (watch for select!)
use tokio_util::sync::CancellationToken; // lab 8 — graceful shutdown

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS}; // lab 9 — MQTT

use rand; // lab 7 — rand::random_range

// =============================================================================
//  PART 0 — CONSTANTS   (lab 1 — const)
//  Every magic number is declared here so nothing is buried in the code.
// =============================================================================

// Sensor periods — Rate Monotonic rule: shorter period = higher RM priority
const GYRO_PERIOD_MS:    u64 = 20;   // 50 Hz  — fastest rate, RM sensor P1
const ACCEL_PERIOD_MS:   u64 = 50;   // 20 Hz  — RM sensor P2
const THERMAL_PERIOD_MS: u64 = 100;  // 10 Hz  — RM sensor P3 / buffer priority 1

// Background RT task periods (Rate Monotonic)
const HEALTH_PERIOD_MS:   u64 = 200;    // RM task P1
const COMPRESS_PERIOD_MS: u64 = 500;    // RM task P2
const ANTENNA_PERIOD_MS:  u64 = 1_000;  // RM task P3 — lowest priority, preemptible

// Safety and QoS thresholds
const JITTER_LIMIT_US:    i64   = 1_000;  // 1 ms — critical sensor jitter ceiling
const MAX_CONSEC_MISSES:  u32   = 3;      // consecutive thermal drops → safety alert
const BUFFER_CAPACITY:    usize = 100;    // bounded sensor buffer
const DEGRADED_THRESHOLD: f32   = 0.80;  // 80 % buffer fill → degraded mode

// Downlink
const VISIBILITY_INTERVAL_S:  u64 = 10;  // simulated orbital pass interval
const DOWNLINK_WINDOW_MS:     u64 = 30;  // entire TX batch must finish within
const DOWNLINK_INIT_LIMIT_MS: u64 = 5;   // radio init must complete within 5 ms

// Fault injection
const FAULT_INTERVAL_S:  u64 = 60;   // inject one fault per minute
const RECOVERY_LIMIT_MS: u64 = 200;  // exceed this → MISSION ABORT

// Simulation
const SIM_DURATION_S: u64 = 180;

// MQTT — same broker as lab 9
const MQTT_BROKER: &str = "broker.emqx.io";
const MQTT_PORT:   u16  = 1883;
const STUDENT_ID:  &str = "tp071542"; // ← change to your student ID

// =============================================================================
//  PART 0 — DATA TYPES   (lab 1 — structs, enums)
// =============================================================================

// Which physical sensor produced a reading  (lab 1 — enum)
#[derive(Debug, Clone, PartialEq)]
enum SensorType
{
    Thermal,
    Accelerometer,
    Gyroscope,
}

// One timestamped sensor reading  (lab 1 — struct)
#[derive(Debug, Clone)]
struct SensorReading
{
    sensor_type:  SensorType,
    value:        f64,
    timestamp_ms: u64,  // UNIX ms — used to measure queue latency
    priority:     u8,   // 1 = highest urgency, 3 = lowest
    sequence_num: u64,
}

// BinaryHeap ordering for SensorReading  (lab 1 — implementing traits)
// Rust's BinaryHeap is max-heap by default.
// We want small priority numbers to pop first (1 = most urgent).
// Reversing other.cmp(self) → self.cmp(other) turns it into a min-heap.
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
    fn cmp(&self, other: &Self) -> std::cmp::Ordering
    {
        other.priority.cmp(&self.priority) // reversed → min-heap on priority
    }
}

// Packet ready for downlink to GCS  (lab 1 — struct)
#[derive(Debug, Clone)]
struct DataPacket
{
    packet_id:     u64,
    payload:       String,
    created_at_ms: u64,   // used to calculate time-in-queue
    size_bytes:    usize,
}

// System state — valid transitions: Normal → Degraded → MissionAbort
// (lab 7 — enum; the typestate *concept* from lab 7 Part 1 is preserved here
//  as a runtime enum because async tasks cannot own generic PhantomData structs.
//  Only valid transitions are made; state is checked before every action.)
#[derive(Debug, Clone, PartialEq)]
enum SystemState
{
    Normal,
    Degraded,      // buffer ≥ 80 % OR transient fault active
    MissionAbort,  // recovery exceeded RECOVERY_LIMIT_MS
}

// Fault variants  (lab 7 — enum with #[derive(Debug)], same as SensorError)
#[derive(Debug)]
enum FaultType
{
    SensorBusHang(u64), // hang duration in ms
    CorruptedReading,
    PowerSpike,
}

// Payload for the MQTT publish channel  (lab 11 — Job struct equivalent)
struct MqttMessage
{
    topic:   String,
    payload: String,
}

// Shared telemetry — all tasks write here under Mutex  (lab 2 — Arc<Mutex<T>>)
#[derive(Default)]
struct SystemMetrics
{
    // Jitter samples per sensor (µs)
    thermal_jitter_us: Vec<i64>,
    accel_jitter_us:   Vec<i64>,
    gyro_jitter_us:    Vec<i64>,

    // Scheduling drift across all tasks (ms)
    drift_ms: Vec<i64>,

    // Throughput
    total_received: u64,
    total_dropped:  u64,
    dropped_log:    Vec<String>,

    // Buffer insert latency (µs)   — lab 3 Instant pattern
    insert_latency_us: Vec<u64>,

    // Deadline violations log
    deadline_violations: Vec<String>,

    // Fault and recovery log
    fault_log:         Vec<String>,
    recovery_times_ms: Vec<u64>,

    // Safety alerts
    safety_alerts:         Vec<String>,
    consec_thermal_misses: u32,

    // CPU utilisation estimate
    active_ms:  u64,
    elapsed_ms: u64,

    // Lab 8 supervisor: gyroscope restart count
    gyro_restarts: u32,
}

// =============================================================================
//  BOUNDED PRIORITY BUFFER
//
//  Wraps BinaryHeap<SensorReading> with a hard capacity limit.
//  push() returns false when full — the caller logs the drop.
//  pop() always returns the highest-priority (smallest priority number) item.
//
//  This IS the Arc<Mutex<Vec<Job>>> job-queue from lab 11 —
//  upgraded from a plain Vec to a BinaryHeap so the highest-priority
//  sensor reading is always drained first by data_compression_task.
// =============================================================================

struct PriorityBuffer
{
    heap:     BinaryHeap<SensorReading>,
    capacity: usize,
}

impl PriorityBuffer
{
    fn new(cap: usize) -> Self
    {
        PriorityBuffer { heap: BinaryHeap::with_capacity(cap), capacity: cap }
    }

    fn push(&mut self, r: SensorReading) -> bool
    {
        if self.heap.len() >= self.capacity { return false; }
        self.heap.push(r);
        true
    }

    fn pop(&mut self) -> Option<SensorReading> { self.heap.pop() }

    fn fill_ratio(&self) -> f32 { self.heap.len() as f32 / self.capacity as f32 }
}

// =============================================================================
//  HELPER FUNCTIONS
// =============================================================================

// Current UNIX time in milliseconds — for log timestamps
fn now_ms() -> u64
{
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

// Print min / max / avg for a Vec<i64>
// Directly mirrors the print_stats() pattern from lab 3
fn print_stat_row(label: &str, samples: &[i64])
{
    if samples.is_empty()
    {
        println!("    {:<34} — no data", label);
        return;
    }
    let min = samples.iter().min().unwrap();
    let max = samples.iter().max().unwrap();
    let avg = samples.iter().sum::<i64>() as f64 / samples.len() as f64;
    println!(
        "    {:<34} n={:>5}  min={:>7}  max={:>7}  avg={:>9.1}",
        label, samples.len(), min, max, avg
    );
}

// =============================================================================
//  MQTT SETUP   (lab 9)
//
//  Follows lab9_client.rs exactly:
//    1. MqttOptions::new(client_id, broker, port)
//    2. mqttoptions.set_keep_alive(...)
//    3. let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10)
//    4. tokio::spawn(async move { while let Ok(_) = eventloop.poll().await {} })
//
//  Extensions beyond lab 9:
//    • also subscribes to the GCS command topic (lab9_server subscribe pattern)
//    • event loop uses tokio::select! (lab 8) so it exits on shutdown
//    • returns the client handle so every task can call .publish()
// =============================================================================

async fn setup_mqtt(shutdown: CancellationToken) -> AsyncClient
{
    // Client ID must be unique — same warning in lab9_client.rs
    let client_id = format!("ocs_{}_{}", STUDENT_ID, now_ms() % 100_000);

    // Step 1 & 2  (lab 9)
    let mut mqttoptions = MqttOptions::new(client_id, MQTT_BROKER, MQTT_PORT);
    mqttoptions.set_keep_alive(Duration::from_secs(5));

    // Step 3  (lab 9)
    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

    // Subscribe to GCS uplink channel  (lab9_server.rs subscribe pattern)
    let cmd_topic = format!("gcs/{}/commands", STUDENT_ID);
    client.subscribe(&cmd_topic, QoS::AtMostOnce).await
          .unwrap_or_else(|e| println!("[MQTT] subscribe error: {e}"));
    println!("[MQTT] Subscribed to GCS commands on '{cmd_topic}'");

    // Step 4  (lab 9) — event loop task, extended with lab 8 select! for shutdown
    tokio::spawn(async move
    {
        loop
        {
            // lab 8 — select! races shutdown vs network event
            tokio::select!
            {
                _ = shutdown.cancelled() => { println!("[MQTT] Event loop exit."); return; }
                result = eventloop.poll() =>
                {
                    match result
                    {
                        // Incoming publish from GCS  (lab9_server.rs receive pattern)
                        Ok(Event::Incoming(Packet::Publish(p))) =>
                        {
                            let msg = String::from_utf8_lossy(&p.payload);
                            println!("[MQTT] GCS → '{}': {}", p.topic, msg);
                        }
                        Ok(_)  => {}  // ACKs, pings — same as lab 9 "keep alive"
                        Err(e) =>
                        {
                            println!("[MQTT] Connection error: {e}. Retrying in 2s...");
                            sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        }
    });

    client
}

// =============================================================================
//  MQTT PUBLISHER TASK   (lab 9 publish + lab 11 mpsc consumer)
//
//  Lab 11 has a dedicated consumer that drains the job queue.
//  This task plays that same role: it receives MqttMessage values
//  from the mpsc channel and publishes them to the broker.
//  Every sensor task is a "producer" — it calls tx.try_send(...)
//  without knowing anything about MQTT internals.
// =============================================================================

async fn mqtt_publisher_task(
    mut rx:   mpsc::Receiver<MqttMessage>, // lab 11 — rx end of channel
    client:   AsyncClient,
    shutdown: CancellationToken,
)
{
    println!("[MQTT-PUB] Ready.");
    loop
    {
        // lab 8 — select! races shutdown vs incoming message
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[MQTT-PUB] Exiting."); return; }
            msg = rx.recv() =>
            {
                match msg
                {
                    Some(m) =>
                    {
                        // lab 9 — client.publish(topic, QoS, retain, payload)
                        let res = client
                            .publish(&m.topic, QoS::AtMostOnce, false, m.payload.as_bytes())
                            .await;
                        // lab 9 — match res { Ok => ..., Err(e) => ... }
                        match res
                        {
                            Ok(_)  => {}
                            Err(e) => println!("[MQTT-PUB] publish error on '{}': {e}", m.topic),
                        }
                    }
                    None => return, // all senders dropped — channel closed
                }
            }
        }
    }
}

// =============================================================================
//  PART 1 — SENSOR ACQUISITION
//
//  Pattern shared by all three sensor tasks:
//    1. tokio::select! { shutdown arm | sleep arm }          — lab 8 / lab 6
//    2. drift = actual_elapsed − expected_elapsed            — lab 2 jitter formula
//    3. buffer.lock().unwrap().push(reading)                 — lab 11 job queue push
//    4. Instant::now() wrapping the push → insert latency    — lab 3
//    5. consecutive miss counter + reset on success          — lab 7
//    6. tx.try_send(MqttMessage { topic, payload })          — lab 11 + lab 9
// =============================================================================

async fn thermal_sensor_task(
    buffer:    Arc<Mutex<PriorityBuffer>>,
    metrics:   Arc<Mutex<SystemMetrics>>,
    state:     Arc<Mutex<SystemState>>,
    emerg_tx:  Arc<watch::Sender<bool>>, // raises antenna preemption (see ARM 2 below)
    mqtt_tx:   mpsc::Sender<MqttMessage>,
    shutdown:  CancellationToken,
)
{
    println!("[Thermal]  period={}ms  buf_priority=1  SAFETY-CRITICAL", THERMAL_PERIOD_MS);
    let mut seq:    u64     = 0;
    let task_start: Instant = Instant::now(); // lab 3 — reference point

    loop
    {
        // lab 8 — select! with two arms: shutdown vs periodic tick
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[Thermal]  exit."); return; }
            _ = sleep(Duration::from_millis(THERMAL_PERIOD_MS)) => {}
        }

        // lab 2 — jitter formula: expected vs actual elapsed
        let expected_ms = seq * THERMAL_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Simulated temperature reading (22 – 37 °C slow ramp)
        let temp: f64 = 22.0 + (seq % 50) as f64 * 0.3;

        let reading = SensorReading
        {
            sensor_type: SensorType::Thermal,
            value:       temp,
            timestamp_ms: now_ms(),
            priority:    1, // highest — always exits buffer before accel / gyro
            sequence_num: seq,
        };

        // lab 3 — time the push to get insert latency
        let t0       = Instant::now();
        let accepted = buffer.lock().unwrap().push(reading);
        let ins_us   = t0.elapsed().as_micros() as u64;

        // lab 2 — lock the shared Mutex to update metrics
        {
            let mut m = metrics.lock().unwrap();
            m.total_received   += 1;
            m.insert_latency_us.push(ins_us);
            m.drift_ms.push(drift_ms);

            if seq > 0
            {
                // jitter = |drift| converted ms → µs
                let jitter_us = (drift_ms * 1_000).unsigned_abs() as i64;
                m.thermal_jitter_us.push(jitter_us);

                if jitter_us > JITTER_LIMIT_US
                {
                    let msg = format!(
                        "[{}ms] [WARN] Thermal jitter {}µs > limit {}µs  seq={}",
                        now_ms(), jitter_us, JITTER_LIMIT_US, seq
                    );
                    println!("{msg}");
                    m.deadline_violations.push(msg);
                }
            }

            // lab 7 — consecutive miss counter, reset on success (glitch_count pattern)
            if accepted
            {
                m.consec_thermal_misses = 0; // reset — same as lab7 run_sensor_loop()
            }
            else
            {
                m.total_dropped         += 1;
                m.consec_thermal_misses += 1;

                let fill = buffer.lock().unwrap().fill_ratio();
                let msg  = format!(
                    "[{}ms] [DROP] Thermal seq={seq}  fill={:.1}%",
                    now_ms(), fill * 100.0
                );
                println!("{msg}");
                m.dropped_log.push(msg);

                // lab 7 — trigger safety alert after N consecutive misses
                if m.consec_thermal_misses >= MAX_CONSEC_MISSES
                {
                    // Raise emergency via watch channel so antenna ARM 2 fires
                    let _ = emerg_tx.send(true);

                    let alert = format!(
                        "[{}ms] !!! SAFETY ALERT !!!  {} consecutive thermal misses",
                        now_ms(), m.consec_thermal_misses
                    );
                    println!("{alert}");
                    m.safety_alerts.push(alert);

                    // lab 9 — publish alert to GCS immediately
                    let _ = mqtt_tx.try_send(MqttMessage
                    {
                        topic:   format!("ocs/{}/alerts", STUDENT_ID),
                        payload: format!(
                            "{{\"event\":\"thermal_alert\",\"misses\":{}}}",
                            m.consec_thermal_misses
                        ),
                    });
                }
            }
        }

        // Transition to Degraded if buffer is too full
        if buffer.lock().unwrap().fill_ratio() >= DEGRADED_THRESHOLD
        {
            let mut s = state.lock().unwrap();
            if *s == SystemState::Normal
            {
                println!("[Thermal]  buf ≥ 80 % → DEGRADED at {}ms", now_ms());
                *s = SystemState::Degraded;
            }
        }

        // lab 9 — publish telemetry every 5 cycles ≈ 500 ms
        if seq % 5 == 0
        {
            let _ = mqtt_tx.try_send(MqttMessage
            {
                topic:   format!("ocs/{}/telemetry/thermal", STUDENT_ID),
                payload: format!(
                    "{{\"seq\":{seq},\"temp\":{temp:.2},\"drift_ms\":{drift_ms}}}"
                ),
            });
        }

        seq += 1;
    }
}

// ─────────────────────────────────────────────────────────────────────────────

async fn accelerometer_task(
    buffer:   Arc<Mutex<PriorityBuffer>>,
    metrics:  Arc<Mutex<SystemMetrics>>,
    mqtt_tx:  mpsc::Sender<MqttMessage>,
    shutdown: CancellationToken,
)
{
    println!("[Accel]    period={}ms  buf_priority=2", ACCEL_PERIOD_MS);
    let mut seq:    u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[Accel]    exit."); return; }
            _ = sleep(Duration::from_millis(ACCEL_PERIOD_MS)) => {}
        }

        let expected_ms = seq * ACCEL_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // Simulate 3-axis acceleration — resultant ≈ 1 g with small wobble
        let ax  = (seq as f64 * 0.05).sin() * 0.10;
        let ay  = (seq as f64 * 0.07).cos() * 0.10;
        let az  = 9.81 + (seq as f64 * 0.03).sin() * 0.01;
        let mag = (ax * ax + ay * ay + az * az).sqrt();

        let reading = SensorReading
        {
            sensor_type:  SensorType::Accelerometer,
            value:        mag,
            timestamp_ms: now_ms(),
            priority:     2,
            sequence_num: seq,
        };

        let t0       = Instant::now(); // lab 3
        let accepted = buffer.lock().unwrap().push(reading);
        let ins_us   = t0.elapsed().as_micros() as u64;

        {
            let mut m = metrics.lock().unwrap();
            m.total_received += 1;
            m.insert_latency_us.push(ins_us);
            m.drift_ms.push(drift_ms);
            if seq > 0
            {
                m.accel_jitter_us.push((drift_ms * 1_000).unsigned_abs() as i64);
            }
            if !accepted
            {
                m.total_dropped += 1;
                m.dropped_log.push(
                    format!("[{}ms] [DROP] Accel seq={seq}", now_ms())
                );
            }
        }

        // lab 9 — publish every 10 cycles ≈ 500 ms
        if seq % 10 == 0
        {
            let _ = mqtt_tx.try_send(MqttMessage
            {
                topic:   format!("ocs/{}/telemetry/accel", STUDENT_ID),
                payload: format!("{{\"seq\":{seq},\"mag\":{mag:.4}}}"),
            });
        }

        seq += 1;
    }
}

// =============================================================================
//  LAB 8 PART 1 — FRAGILE TASK
//
//  Direct equivalent of fragile_worker() from lab 8.
//  Does real gyroscope work but has a small random chance to panic —
//  simulating a hardware bus fault or watchdog timeout.
//  No shutdown token: the supervisor (below) owns its lifecycle,
//  exactly as in the lab 8 unsupervised-panic example.
// =============================================================================

async fn fragile_gyroscope(
    generation: u32,
    buffer:     Arc<Mutex<PriorityBuffer>>,
    metrics:    Arc<Mutex<SystemMetrics>>,
    mqtt_tx:    mpsc::Sender<MqttMessage>,
)
{
    println!("[Gyro-{generation}]  period={}ms  buf_priority=3", GYRO_PERIOD_MS);
    let mut seq:    u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        sleep(Duration::from_millis(GYRO_PERIOD_MS)).await; // lab 6

        // lab 7 — rand::random_range (same function used in lab7 for roll)
        // lab 8 — 3 % chance to panic, same as fragile_worker's "roll < 2 of 10"
        if rand::random_range(0u32..100) < 3
        {
            println!("[Gyro-{generation}]  hardware fault — panicking!");
            panic!("Gyroscope hardware fault (generation {generation})");
        }

        let expected_ms = seq * GYRO_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        let omega_z: f64 = 0.5 * (seq as f64 * 0.10).sin(); // angular rate deg/s

        let reading = SensorReading
        {
            sensor_type:  SensorType::Gyroscope,
            value:        omega_z,
            timestamp_ms: now_ms(),
            priority:     3,
            sequence_num: seq,
        };

        let t0       = Instant::now();
        let accepted = buffer.lock().unwrap().push(reading);
        let ins_us   = t0.elapsed().as_micros() as u64;

        {
            let mut m = metrics.lock().unwrap();
            m.total_received += 1;
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

        if seq % 25 == 0 // every 25 × 20 ms = 500 ms  (lab 9)
        {
            let _ = mqtt_tx.try_send(MqttMessage
            {
                topic:   format!("ocs/{}/telemetry/gyro", STUDENT_ID),
                payload: format!(
                    "{{\"gen\":{generation},\"seq\":{seq},\"omega_z\":{omega_z:.4}}}"
                ),
            });
        }

        seq += 1;
    }
}

// =============================================================================
//  LAB 8 PART 2 — SUPERVISOR
//
//  Direct equivalent of the supervisor loop in lab 8 Part 2.
//  Spawns fragile_gyroscope; if it panics the supervisor restarts it.
//  Uses the same match-on-JoinError pattern:
//    match handle.await {
//        Ok(_)               => normal exit
//        Err(e) if e.is_panic() => restart
//        Err(_)              => cancelled
//    }
// =============================================================================

async fn gyro_supervisor_task(
    buffer:   Arc<Mutex<PriorityBuffer>>,
    metrics:  Arc<Mutex<SystemMetrics>>,
    mqtt_tx:  mpsc::Sender<MqttMessage>,
    shutdown: CancellationToken,
)
{
    println!("[Gyro-SUP] Supervisor online — will restart gyro on panic.");
    let mut generation: u32 = 0;

    loop
    {
        // Supervisor also checks shutdown before spawning a new generation
        if shutdown.is_cancelled() { println!("[Gyro-SUP] exit."); return; }

        generation += 1;
        println!("[Gyro-SUP] Starting generation {generation}...");

        // lab 8 — spawn + await the fragile task
        let handle = tokio::spawn(fragile_gyroscope(
            generation,
            Arc::clone(&buffer),  // lab 2 — Arc::clone
            Arc::clone(&metrics),
            mqtt_tx.clone(),      // lab 11 — clone the sender
        ));

        // lab 8 — same match pattern as lab 8's supervisor loop
        match handle.await
        {
            Ok(_) =>
            {
                println!("[Gyro-SUP] Gyro exited normally — supervisor done.");
                return;
            }
            Err(e) if e.is_panic() =>
            {
                {
                    let mut m = metrics.lock().unwrap();
                    m.gyro_restarts += 1;
                }
                println!("[Gyro-SUP] Gyro panicked! Restarting in 1s...");

                // lab 9 — notify GCS of the restart
                let _ = mqtt_tx.try_send(MqttMessage
                {
                    topic:   format!("ocs/{}/alerts", STUDENT_ID),
                    payload: format!(
                        "{{\"event\":\"gyro_restart\",\"generation\":{generation}}}"
                    ),
                });

                sleep(Duration::from_secs(1)).await; // backoff — lab 8 pattern
            }
            Err(_) => { println!("[Gyro-SUP] Gyro cancelled — exit."); return; }
        }
    }
}

// =============================================================================
//  PART 2 — RATE MONOTONIC BACKGROUND TASKS
//
//  RM priority assignment (shorter period = higher priority):
//    health_monitor    200 ms  →  RM P1
//    data_compression  500 ms  →  RM P2
//    antenna_alignment 1 s     →  RM P3  (preemptible)
//
//  Soft-RTS preemption via tokio::select! (lab 8):
//  antenna_alignment_task races THREE futures at once:
//    ARM 1  shutdown.cancelled()      — graceful exit
//    ARM 2  emerg_rx.changed()        — thermal/fault emergency → skip cycle
//    ARM 3  sleep(ANTENNA_PERIOD_MS)  — normal work tick
//  If the thermal emergency is raised WHILE the antenna is sleeping,
//  ARM 2 wins immediately — truly reactive, not polled.
//  This is cooperative preemption: we cannot hard-preempt but we
//  react at the next yield point (.await), which is sub-millisecond.
// =============================================================================

async fn health_monitor_task(
    buffer:   Arc<Mutex<PriorityBuffer>>,
    metrics:  Arc<Mutex<SystemMetrics>>,
    state:    Arc<Mutex<SystemState>>,
    mqtt_tx:  mpsc::Sender<MqttMessage>,
    shutdown: CancellationToken,
)
{
    // lab 6 — heartbeat pattern (periodic task)
    println!("[Health]   period={}ms  RM-P1", HEALTH_PERIOD_MS);
    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        let t0 = Instant::now(); // lab 3 — time active work

        // lab 8 — select!
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[Health]   exit."); return; }
            _ = sleep(Duration::from_millis(HEALTH_PERIOD_MS)) => {}
        }

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

        // RM P1 deadline: drift should be < 5 ms
        if drift_ms.abs() > 5
        {
            let v = format!(
                "[{}ms] [DEADLINE] Health drift={drift_ms:+}ms iter={iter}",
                now_ms()
            );
            println!("{v}");
            metrics.lock().unwrap().deadline_violations.push(v);
        }

        // lab 9 — publish status to GCS
        let _ = mqtt_tx.try_send(MqttMessage
        {
            topic:   format!("ocs/{}/status", STUDENT_ID),
            payload: format!(
                "{{\"iter\":{iter},\"fill\":{:.1},\"state\":\"{sys_state:?}\",\"drift_ms\":{drift_ms}}}",
                fill * 100.0
            ),
        });

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

// ─────────────────────────────────────────────────────────────────────────────

async fn data_compression_task(
    buffer:   Arc<Mutex<PriorityBuffer>>,
    dq:       Arc<Mutex<Vec<DataPacket>>>, // lab 11 — Arc<Mutex<Vec>> downlink queue
    metrics:  Arc<Mutex<SystemMetrics>>,
    shutdown: CancellationToken,
)
{
    println!("[Compress] period={}ms  RM-P2", COMPRESS_PERIOD_MS);
    let mut pkt_id: u64     = 0;
    let mut iter:   u64     = 0;
    let task_start: Instant = Instant::now();

    loop
    {
        let t0 = Instant::now();

        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[Compress] exit."); return; }
            _ = sleep(Duration::from_millis(COMPRESS_PERIOD_MS)) => {}
        }

        let expected_ms = iter * COMPRESS_PERIOD_MS;
        let actual_ms   = task_start.elapsed().as_millis() as u64;
        let drift_ms    = actual_ms as i64 - expected_ms as i64;

        // lab 11 — drain the shared job queue with .pop()
        // BinaryHeap ordering ensures thermal (priority 1) exits first
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
            // Serialise manually — no serde, keep it tutorial-aligned (lab 9 style)
            let data_json: String = batch.iter()
                .map(|r| format!(
                    "{{\"s\":\"{:?}\",\"v\":{:.4},\"t\":{}}}",
                    r.sensor_type, r.value, r.timestamp_ms
                ))
                .collect::<Vec<_>>()
                .join(",");

            let raw  = format!(
                "{{\"pkt\":{pkt_id},\"n\":{},\"ts\":{},\"d\":[{data_json}]}}",
                batch.len(), now_ms()
            );
            let size = (raw.len() / 2).max(1); // ~50 % compression

            let t1 = Instant::now(); // lab 3 — queue insert latency
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

        let active_ms = t0.elapsed().as_millis() as u64;
        {
            let mut m = metrics.lock().unwrap();
            m.drift_ms.push(drift_ms);
            m.active_ms += active_ms;
        }
        iter += 1;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  THREE-ARM select! — the key lab 8 pattern for soft preemption
// ─────────────────────────────────────────────────────────────────────────────

async fn antenna_alignment_task(
    metrics:  Arc<Mutex<SystemMetrics>>,
    state:    Arc<Mutex<SystemState>>,
    emerg_rx: watch::Receiver<bool>, // ARM 2 — emergency preemption signal
    shutdown: CancellationToken,
)
{
    println!("[Antenna]  period={}ms  RM-P3  (soft-preemptible via select!)", ANTENNA_PERIOD_MS);
    let mut iter:   u64                   = 0;
    let task_start: Instant               = Instant::now();
    let mut emerg:  watch::Receiver<bool> = emerg_rx; // mut needed for .changed()

    loop
    {
        let t0 = Instant::now();

        // lab 8 — THREE-ARM tokio::select!
        // Whichever future resolves first wins; the others are dropped.
        tokio::select!
        {
            // ARM 1 — graceful shutdown  (lab 8 CancellationToken pattern)
            _ = shutdown.cancelled() =>
            {
                println!("[Antenna]  Shutdown signal — exiting gracefully.");
                return;
            }

            // ARM 2 — thermal / fault emergency PREEMPTION
            // watch::Receiver::changed() is a Future that resolves the moment
            // any writer calls .send() on the watch channel.
            // If thermal raises the emergency DURING our 1000 ms sleep,
            // this arm fires immediately — sub-millisecond reaction time.
            result = emerg.changed() =>
            {
                if result.is_ok() && *emerg.borrow()
                {
                    println!(
                        "[Antenna]  iter={iter:>4}  PREEMPTED by emergency at {}ms",
                        now_ms()
                    );
                    let v = format!(
                        "[{}ms] [PREEMPT] Antenna iter={iter} yielded for thermal/fault",
                        now_ms()
                    );
                    metrics.lock().unwrap().deadline_violations.push(v);
                }
                // skip this cycle regardless of true/false
                iter += 1;
                continue;
            }

            // ARM 3 — normal 1 s scheduled work tick  (lab 6 heartbeat pattern)
            _ = sleep(Duration::from_millis(ANTENNA_PERIOD_MS)) =>
            {
                // Belt-and-suspenders: check state in case emergency fired
                // just before we entered this cycle
                if *emerg.borrow() || *state.lock().unwrap() == SystemState::MissionAbort
                {
                    println!("[Antenna]  iter={iter:>4}  Emergency/abort active — skipping.");
                    iter += 1;
                    continue;
                }

                let expected_ms = iter * ANTENNA_PERIOD_MS;
                let actual_ms   = task_start.elapsed().as_millis() as u64;
                let drift_ms    = actual_ms as i64 - expected_ms as i64;

                let az = (iter as f64 * 7.2) % 360.0;
                let el = 30.0 + 20.0 * (iter as f64 * 0.05).sin();

                println!(
                    "[Antenna]  iter={iter:>4}  az={az:>6.1}°  el={el:>5.1}°  \
                     drift={drift_ms:+}ms"
                );

                // RM P3 deadline: drift < 20 ms
                if drift_ms.abs() > 20
                {
                    let v = format!(
                        "[{}ms] [DEADLINE] Antenna drift={drift_ms:+}ms iter={iter}",
                        now_ms()
                    );
                    println!("{v}");
                    metrics.lock().unwrap().deadline_violations.push(v);
                }

                let active_ms = t0.elapsed().as_millis() as u64;
                {
                    let mut m = metrics.lock().unwrap();
                    m.drift_ms.push(drift_ms);
                    m.active_ms += active_ms;
                }
            }
        }

        iter += 1;
    }
}

// =============================================================================
//  PART 3 — DOWNLINK DATA MANAGEMENT
//
//  Wakes every VISIBILITY_INTERVAL_S seconds (simulated orbital pass).
//  Drains the downlink queue (lab 11 — Arc<Mutex<Vec>>.drain()),
//  enforces the 30 ms transmission window (lab 3 — Instant timing),
//  and publishes each packet to GCS via MQTT (lab 9).
// =============================================================================

async fn downlink_task(
    dq:       Arc<Mutex<Vec<DataPacket>>>,
    metrics:  Arc<Mutex<SystemMetrics>>,
    state:    Arc<Mutex<SystemState>>,
    mqtt_tx:  mpsc::Sender<MqttMessage>,
    shutdown: CancellationToken,
)
{
    println!(
        "[Downlink] window_interval={}s  tx_limit={}ms  init_limit={}ms",
        VISIBILITY_INTERVAL_S, DOWNLINK_WINDOW_MS, DOWNLINK_INIT_LIMIT_MS
    );

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[Downlink] exit."); return; }
            _ = sleep(Duration::from_secs(VISIBILITY_INTERVAL_S)) => {}
        }

        let window_t = Instant::now(); // lab 3 — window deadline timer
        println!("\n[Downlink] === Visibility window {}ms ===", now_ms());

        let q_fill = dq.lock().unwrap().len() as f32 / BUFFER_CAPACITY as f32;
        println!("[Downlink] Queue fill: {:.1}%", q_fill * 100.0);

        if q_fill >= DEGRADED_THRESHOLD
        {
            let mut s = state.lock().unwrap();
            if *s == SystemState::Normal
            {
                println!("[Downlink] Queue ≥ 80 % → DEGRADED");
                *s = SystemState::Degraded;
            }
        }

        // Simulate radio init (3 ms)
        sleep(Duration::from_millis(3)).await;
        let init_ms = window_t.elapsed().as_millis() as u64;

        if init_ms > DOWNLINK_INIT_LIMIT_MS
        {
            let v = format!(
                "[{}ms] [DEADLINE] Radio init {init_ms}ms > limit {DOWNLINK_INIT_LIMIT_MS}ms",
                now_ms()
            );
            println!("{v}");
            metrics.lock().unwrap().deadline_violations.push(v);
            continue;
        }

        println!("[Downlink] Init OK ({init_ms}ms). Transmitting...");

        // lab 11 — drain the shared Vec (same as queue.pop() loop)
        let packets: Vec<DataPacket> = dq.lock().unwrap().drain(..).collect();
        if packets.is_empty() { println!("[Downlink] Nothing queued."); continue; }

        let tx_t        = Instant::now();
        let mut tx_done = 0usize;
        let mut total_b = 0usize;

        for pkt in &packets
        {
            // Enforce 30 ms window
            if tx_t.elapsed().as_millis() as u64 >= DOWNLINK_WINDOW_MS
            {
                let v = format!(
                    "[{}ms] [DEADLINE] TX window exceeded after {tx_done} packets",
                    now_ms()
                );
                println!("{v}");
                metrics.lock().unwrap().deadline_violations.push(v);
                break;
            }

            let q_lat = now_ms().saturating_sub(pkt.created_at_ms);
            total_b  += pkt.size_bytes;
            tx_done  += 1;

            println!(
                "[Downlink] TX pkt={}  {}B  queue_lat={}ms",
                pkt.packet_id, pkt.size_bytes, q_lat
            );

            // lab 9 — publish to GCS (this IS the IPC channel the assignment requires)
            let _ = mqtt_tx.try_send(MqttMessage
            {
                topic:   format!("ocs/{}/downlink", STUDENT_ID),
                payload: format!(
                    "{{\"pkt\":{},\"bytes\":{},\"q_lat_ms\":{}}}",
                    pkt.packet_id, pkt.size_bytes, q_lat
                ),
            });
        }

        let tx_ms = tx_t.elapsed().as_millis() as u64;
        println!(
            "[Downlink] Done  {tx_done}/{} pkts  {total_b}B  {tx_ms}ms",
            packets.len()
        );
    }
}

// =============================================================================
//  PART 4 — FAULT INJECTION & RECOVERY
//
//  lab 7 — enum FaultType { ... } + match arms (mirrors SensorError pattern)
//  lab 3 — Instant::now() to time recovery duration
//  lab 9 — publish fault events to GCS
// =============================================================================

async fn fault_injector_task(
    metrics:  Arc<Mutex<SystemMetrics>>,
    state:    Arc<Mutex<SystemState>>,
    emerg_tx: Arc<watch::Sender<bool>>,
    mqtt_tx:  mpsc::Sender<MqttMessage>,
    shutdown: CancellationToken,
)
{
    println!(
        "[Faults]   interval={}s  recovery_limit={}ms",
        FAULT_INTERVAL_S, RECOVERY_LIMIT_MS
    );
    let mut count: u32 = 0;

    loop
    {
        tokio::select!
        {
            _ = shutdown.cancelled() => { println!("[Faults]   exit."); return; }
            _ = sleep(Duration::from_secs(FAULT_INTERVAL_S)) => {}
        }

        count += 1;

        // lab 7 — rand::random_range + match on enum variant
        let fault = match rand::random_range(0u32..3)
        {
            0 => FaultType::SensorBusHang(80),
            1 => FaultType::CorruptedReading,
            _ => FaultType::PowerSpike,
        };

        let fmsg = format!("[{}ms] [FAULT #{count}] {fault:?}", now_ms());
        println!("\n{fmsg}");
        metrics.lock().unwrap().fault_log.push(fmsg);

        // lab 9 — alert the GCS
        let _ = mqtt_tx.try_send(MqttMessage
        {
            topic:   format!("ocs/{}/alerts", STUDENT_ID),
            payload: format!("{{\"event\":\"fault\",\"n\":{count},\"type\":\"{fault:?}\"}}"),
        });

        // lab 7 — match arms, same style as match read_sensor()
        match fault
        {
            FaultType::SensorBusHang(ms) =>
            {
                println!("[Faults]   Simulating {ms}ms bus hang...");
                sleep(Duration::from_millis(ms)).await;
            }
            FaultType::CorruptedReading =>
            {
                println!("[Faults]   Corrupted value injected — downstream must detect NaN");
            }
            FaultType::PowerSpike =>
            {
                println!("[Faults]   Power spike → DEGRADED + raising emergency");
                *state.lock().unwrap() = SystemState::Degraded;
                let _ = emerg_tx.send(true); // wakes antenna ARM 2
            }
        }

        // lab 3 — time the recovery
        let t0 = Instant::now();
        println!("[Faults]   Recovering...");
        sleep(Duration::from_millis(50)).await; // simulated subsystem reset

        let _ = emerg_tx.send(false); // clear emergency
        *state.lock().unwrap() = SystemState::Normal;

        let rec_ms = t0.elapsed().as_millis() as u64;
        println!("[Faults]   Recovery complete in {rec_ms}ms");

        {
            let mut m = metrics.lock().unwrap();
            m.recovery_times_ms.push(rec_ms);

            if rec_ms > RECOVERY_LIMIT_MS
            {
                let abort = format!(
                    "[{}ms] !!! MISSION ABORT !!!  recovery {rec_ms}ms > limit {RECOVERY_LIMIT_MS}ms",
                    now_ms()
                );
                println!("{abort}");
                m.safety_alerts.push(abort);
                *state.lock().unwrap() = SystemState::MissionAbort;
            }
        }
    }
}

// =============================================================================
//  METRICS REPORTER  — live snapshot every 10 s
// =============================================================================

async fn metrics_reporter_task(
    metrics:  Arc<Mutex<SystemMetrics>>,
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
        print_report(&metrics.lock().unwrap());
    }
}

fn print_report(m: &SystemMetrics)
{
    let loss = if m.total_received > 0
        { m.total_dropped as f64 / m.total_received as f64 * 100.0 }
        else { 0.0 };
    let cpu = if m.elapsed_ms > 0
        { m.active_ms as f64 / m.elapsed_ms as f64 * 100.0 }
        else { 0.0 };

    println!("\n╔══════════════════════════════════════════════════════╗");
    println!(  "║  OCS LIVE REPORT  at {}ms", now_ms());
    println!(  "╠══════════════════════════════════════════════════════╣");
    println!("║  Received: {}  Dropped: {} ({:.2}% loss)  Gyro restarts: {}",
             m.total_received, m.total_dropped, loss, m.gyro_restarts);
    println!("║");
    println!("║  Jitter (µs)  limit={}µs", JITTER_LIMIT_US);
    print_stat_row("Thermal [SAFETY-CRITICAL]", &m.thermal_jitter_us);
    print_stat_row("Accelerometer",              &m.accel_jitter_us);
    print_stat_row("Gyroscope",                  &m.gyro_jitter_us);
    println!("║");
    println!("║  Scheduling drift (ms)");
    print_stat_row("All tasks", &m.drift_ms);
    println!("║");
    println!("║  Insert latency (µs)");
    let lat: Vec<i64> = m.insert_latency_us.iter().map(|&v| v as i64).collect();
    print_stat_row("Sensor → buffer.push()", &lat);
    println!("║");
    println!("║  Deadline violations: {}", m.deadline_violations.len());
    for v in m.deadline_violations.iter().take(3) { println!("║    {v}"); }
    if m.deadline_violations.len() > 3
    {
        println!("║    … and {} more", m.deadline_violations.len() - 3);
    }
    println!("║");
    println!("║  Faults injected: {}", m.fault_log.len());
    for f in &m.fault_log { println!("║    {f}"); }
    if !m.recovery_times_ms.is_empty()
    {
        let max_r = m.recovery_times_ms.iter().max().unwrap();
        let avg_r = m.recovery_times_ms.iter().sum::<u64>() as f64
                    / m.recovery_times_ms.len() as f64;
        println!("║  Recovery: max={max_r}ms  avg={avg_r:.1}ms");
    }
    println!("║");
    println!("║  CPU utilisation ≈ {cpu:.2}%");
    if !m.safety_alerts.is_empty()
    {
        println!("║");
        println!("║  SAFETY ALERTS:");
        for a in &m.safety_alerts { println!("║    {a}"); }
    }
    println!("╚══════════════════════════════════════════════════════╝\n");
}

// =============================================================================
//  MAIN   (lab 6 — #[tokio::main] + tokio::spawn)
// =============================================================================

#[tokio::main]
async fn main()
{
    println!("╔═══════════════════════════════════════════════════════╗");
    println!("║  OCS — Satellite Onboard Control System               ║");
    println!("║  CT087-3-3  |  Student A  |  Soft RTS on Tokio        ║");
    println!("╚═══════════════════════════════════════════════════════╝\n");

    // ── Shared state  (lab 2 — Arc<Mutex<T>>)
    let buffer  = Arc::new(Mutex::new(PriorityBuffer::new(BUFFER_CAPACITY)));
    let metrics = Arc::new(Mutex::new(SystemMetrics::default()));
    let state   = Arc::new(Mutex::new(SystemState::Normal));
    let dq      = Arc::new(Mutex::new(Vec::<DataPacket>::new())); // lab 11 job queue

    // ── Emergency watch channel for antenna preemption
    // watch::channel(initial_value) → (Sender, Receiver)
    // watch::Receiver::changed() is an async Future that resolves whenever
    // the value changes — this lets ARM 2 of select! react sub-millisecond
    // without polling. Both thermal_sensor_task and fault_injector_task need
    // to write it, so the Sender is wrapped in Arc (lab 2 pattern).
    let (emerg_tx, emerg_rx) = watch::channel(false);
    let emerg_tx = Arc::new(emerg_tx);

    // ── CancellationToken  (lab 8 — graceful shutdown)
    // .clone() is cheap — all tasks share the same cancellation signal
    let shutdown = CancellationToken::new();

    // ── MQTT internal queue  (lab 11 mpsc channel)
    // tokio::sync::mpsc is the async equivalent of std::sync::mpsc from lab 11.
    // Producers call tx.try_send(MqttMessage{..}) — same as lab11 tx_clone.send().
    // The dedicated consumer (mqtt_publisher_task) drains it and calls publish().
    let (mqtt_tx, mqtt_rx) = mpsc::channel::<MqttMessage>(50);

    // ── Connect MQTT  (lab 9)
    let mqtt_client = setup_mqtt(shutdown.clone()).await;

    println!("[OCS] Config:");
    println!("      Buffer capacity : {BUFFER_CAPACITY}");
    println!("      Degraded at     : {:.0}%", DEGRADED_THRESHOLD * 100.0);
    println!("      Jitter limit    : {JITTER_LIMIT_US}µs");
    println!("      Recovery limit  : {RECOVERY_LIMIT_MS}ms");
    println!("      MQTT broker     : {MQTT_BROKER}:{MQTT_PORT}");
    println!("      Publish prefix  : ocs/{STUDENT_ID}/{{telemetry|alerts|status|downlink}}");
    println!("      Subscribe       : gcs/{STUDENT_ID}/commands");
    println!("      Sim duration    : {SIM_DURATION_S}s\n");

    // ── Spawn all subsystems  (lab 6 — tokio::spawn)
    // Arc::clone() on every argument — lab 2 pattern (same as Arc::clone(&data))

    // MQTT publisher — consumes the channel, owns the AsyncClient
    tokio::spawn(mqtt_publisher_task(
        mqtt_rx, mqtt_client, shutdown.clone(),
    ));

    // Part 1: sensors
    tokio::spawn(thermal_sensor_task(
        Arc::clone(&buffer), Arc::clone(&metrics), Arc::clone(&state),
        Arc::clone(&emerg_tx), mqtt_tx.clone(), shutdown.clone(),
    ));
    tokio::spawn(accelerometer_task(
        Arc::clone(&buffer), Arc::clone(&metrics),
        mqtt_tx.clone(), shutdown.clone(),
    ));
    // Lab 8 supervisor wraps the fragile gyroscope
    tokio::spawn(gyro_supervisor_task(
        Arc::clone(&buffer), Arc::clone(&metrics),
        mqtt_tx.clone(), shutdown.clone(),
    ));

    // Part 2: RM background tasks (spawned in RM priority order)
    tokio::spawn(health_monitor_task(
        Arc::clone(&buffer), Arc::clone(&metrics), Arc::clone(&state),
        mqtt_tx.clone(), shutdown.clone(),
    ));
    tokio::spawn(data_compression_task(
        Arc::clone(&buffer), Arc::clone(&dq),
        Arc::clone(&metrics), shutdown.clone(),
    ));
    tokio::spawn(antenna_alignment_task(
        Arc::clone(&metrics), Arc::clone(&state),
        emerg_rx,          // watch::Receiver for ARM 2 of select!
        shutdown.clone(),
    ));

    // Part 3: downlink — MQTT is the IPC channel to the GCS
    tokio::spawn(downlink_task(
        Arc::clone(&dq), Arc::clone(&metrics),
        Arc::clone(&state), mqtt_tx.clone(), shutdown.clone(),
    ));

    // Part 4: fault injection
    tokio::spawn(fault_injector_task(
        Arc::clone(&metrics), Arc::clone(&state),
        Arc::clone(&emerg_tx), mqtt_tx.clone(), shutdown.clone(),
    ));

    // Live metrics reporter
    tokio::spawn(metrics_reporter_task(
        Arc::clone(&metrics), shutdown.clone(),
    ));

    println!("[OCS] All 10 tasks online. Running for {SIM_DURATION_S}s...\n");

    // ── Run for SIM_DURATION_S then shut down  (lab 8 — CancellationToken)
    sleep(Duration::from_secs(SIM_DURATION_S)).await;

    println!("\n[OCS] Time elapsed — requesting graceful shutdown...");
    shutdown.cancel(); // every tokio::select! shutdown arm fires

    sleep(Duration::from_millis(500)).await; // let tasks log their exits

    println!("\n[OCS] ═══ FINAL REPORT ═══");
    print_report(&metrics.lock().unwrap());
    println!("[OCS] Done.");
}