# RTS Assignment – SatelliteGroundControlSystem for CubeSat

This project is a **Rust-based real-time system simulation**, consisting of two separate components:

* [HARD RTOS] **OCS (SATELLITE ONBOARD CONTROL SYSTEM)** – BY LUVEN MARK (TP071542)
* [SOFT RTS]  **GCS (GROUND CONTROL STATION)** – BY CHONG CHUN KIT (TP077436)

Both systems run independently and communicate during the simulation.

---
## How to Run

This project uses **two separate binaries**, so the systems should be run in **two different terminals**.

### Terminal 1 – Start OCS first
```bash
cargo run --bin OCS --release
```

### Terminal 2 – Start GCS second
```bash
cargo run --bin GCS --release
```

* Both components are designed as **separate real-time simulation systems**.
* Logs and performance metrics will be printed in a log file and console during execution.

