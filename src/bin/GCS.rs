// GROUND CONTROL STATION - BY CHONG CHUN KIT (TP077436)
// SOFT RTS

use std::time::Instant;
use std::hint::black_box;
use std::time::Duration;

const BUFFER_SIZE: usize = 1024; // 1KB
const NUM_SAMPLES: usize = 5_000; // Collect 5000 samples

// 1. STACK ALLOCATION (Predictable)
// The [u8; BUFFER_SIZE] array is created on the stack.
// This is just moving the stack pointer, which is one CPU instruction.
fn create_on_stack() {
    let _buffer: [u8; BUFFER_SIZE] = [0; BUFFER_SIZE];

    // We "black_box" the buffer to ensure the compiler
    // doesn't optimize this entire function away.
    black_box(_buffer);             // Zero Cost Abstraction - prevents optimization away
}

// 2. HEAP ALLOCATION (Unpredictable)
// This Vec<u8> is created on the heap.
// This requires a call to the OS malloc() function,
// which must search for a free block of memory.
fn create_on_heap() {
    let _buffer: Vec<u8> = Vec::with_capacity(BUFFER_SIZE);
    black_box(_buffer);
}

fn main() {
    let mut stack_samples: Vec<u128> = Vec::with_capacity(NUM_SAMPLES);
    let mut heap_samples: Vec<u128> = Vec::with_capacity(NUM_SAMPLES);

    println!("Measuring stack allocation...");
    for _ in 0..NUM_SAMPLES {
        let start: Instant = Instant::now();
        create_on_stack();
        let duration: Duration = start.elapsed(); // elapsed time
        stack_samples.push(duration.as_nanos()); // push duration in nanoseconds
    }

    println!("Measuring heap allocation...");
    for _ in 0..NUM_SAMPLES {
        let start: Instant = Instant::now();
        create_on_heap();
        let duration: Duration = start.elapsed();
        heap_samples.push(duration.as_nanos());
    }

    println!("\n--- Stack Allocation Stats ---");
    print_stats(&stack_samples);

    println!("\n--- Heap Allocation Stats ---");
    print_stats(&heap_samples);
}

// Helper function to print statistics for a slice of samples
fn print_stats(samples: &Vec<u128>) {
    if samples.is_empty() {return; }

    let min = samples.iter().min().unwrap(); // iteration minimum unwrap
    let max = samples.iter().max().unwrap();
    let sum: u128 = samples.iter().sum();
    let avg = sum as f64 / samples.len() as f64;

    // Jitter is the difference between worst and best case
    let jitter = max - min;

    println!("Samples: {}", samples.len());
    println!("Min: {} ns", min);
    println!("Max: {} ns (Observed WCET)", max);
    println!("Avg: {:.2} ns", avg);
    println!("Jitter: {} ns (Max - Min)", jitter);
}

