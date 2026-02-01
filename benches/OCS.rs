// SATELLITE ONBOARD CONTROL SYSTEM - BY LUVEN MARK (TP071542)
// HARD RTS BENCHMARK

use criterion::{criterion_group, criterion_main, Criterion, black_box};
use std::hint;

const BUFFER_SIZE: usize = 1024; // 1KB

// 1. STACK ALLOCATION
fn create_on_stack() {
    let _buffer: [u8; BUFFER_SIZE] = [0; BUFFER_SIZE];
    hint::black_box(_buffer);
}

// 2. HEAP ALLOCATION
fn create_on_heap() {
    let _buffer: Vec<u8> = Vec::with_capacity(BUFFER_SIZE); // Best Case Heap Allocation
    hint::black_box(_buffer);
}

fn push_to_vec() {
    let mut vec = Vec::new(); // No capacity, Worst Case Heap Allocation
    vec.push(1);
    black_box(vec);
}

// --- This is the Criterion benchmark harness ---

fn benchmark_allocations(c: &mut Criterion) {
    let mut group = c.benchmark_group("Stack vs Heap");
    
    // Benchmark the stack function
    group.bench_function("Stack", |b| b.iter(|| create_on_stack()));
    
    // Benchmark the heap function
    group.bench_function("Heap", |b| b.iter(|| create_on_heap()));
    
    group.finish();
}

criterion_group!(benches, benchmark_allocations);
criterion_main!(benches);