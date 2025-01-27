// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

//! This module will be compiled when it's either linux_x86 or linux_x86_64.

use libc::{cpu_set_t, sched_setaffinity, CPU_SET};
use std::io::prelude::*;
use std::io::BufReader;
use std::mem::{size_of, zeroed, MaybeUninit};
use std::time::Instant;
use std::{cell::UnsafeCell, fs::read_to_string};

type Error = Box<dyn std::error::Error>;

static TSC_STATE: TSCState = TSCState {
    is_tsc_available: UnsafeCell::new(false),
    tsc_level: UnsafeCell::new(TSCLevel::Unstable),
    nanos_per_cycle: UnsafeCell::new(1.0),
};

struct TSCState {
    is_tsc_available: UnsafeCell<bool>,
    tsc_level: UnsafeCell<TSCLevel>,
    nanos_per_cycle: UnsafeCell<f64>,
}

unsafe impl Sync for TSCState {}

#[ctor::ctor]
unsafe fn init() {
    let tsc_level = TSCLevel::get();
    let is_tsc_available = match &tsc_level {
        TSCLevel::Stable { .. } => true,
        TSCLevel::PerCPUStable { .. } => true,
        TSCLevel::Unstable => false,
    };
    if is_tsc_available {
        *TSC_STATE.nanos_per_cycle.get() = 1_000_000_000.0 / tsc_level.cycles_per_second() as f64;
    }
    *TSC_STATE.is_tsc_available.get() = is_tsc_available;
    *TSC_STATE.tsc_level.get() = tsc_level;
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

#[inline]
pub(crate) fn is_tsc_available() -> bool {
    unsafe { *TSC_STATE.is_tsc_available.get() }
}

#[inline]
pub(crate) fn get_tsc_level() -> TSCLevel {
    unsafe { (*TSC_STATE.tsc_level.get()).clone() }
}

#[inline]
pub(crate) fn nanos_per_cycle() -> f64 {
    unsafe { *TSC_STATE.nanos_per_cycle.get() }
}

#[inline]
pub(crate) fn current_cycle() -> u64 {
    match unsafe { &*TSC_STATE.tsc_level.get() } {
        TSCLevel::Stable {
            cycles_from_anchor, ..
        } => tsc().wrapping_sub(*cycles_from_anchor),
        TSCLevel::PerCPUStable {
            cycles_from_anchor, ..
        } => {
            let (tsc, cpuid) = tsc_with_cpuid();
            let anchor = cycles_from_anchor[cpuid];
            tsc.wrapping_sub(anchor)
        }
        TSCLevel::Unstable => panic!("tsc is unstable"),
    }
}

#[derive(Debug, Clone)]
pub enum TSCLevel {
    Stable {
        cycles_per_second: u64,
        cycles_from_anchor: u64,
    },
    PerCPUStable {
        cycles_per_second: u64,
        cycles_from_anchor: Vec<u64>,
    },
    Unstable,
}

impl TSCLevel {
    fn get() -> TSCLevel {
        let anchor = Instant::now();
        if is_tsc_stable() {
            let (cps, cfa) = cycles_per_sec(anchor);
            return TSCLevel::Stable {
                cycles_per_second: cps,
                cycles_from_anchor: cfa,
            };
        }

        if is_tsc_percpu_stable() {
            // Retrieve the IDs of all active CPUs.
            let cpuids = if let Ok(cpuids) = available_cpus() {
                if cpuids.is_empty() {
                    return TSCLevel::Unstable;
                }

                cpuids
            } else {
                return TSCLevel::Unstable;
            };

            let max_cpu_id = *cpuids.iter().max().unwrap();

            // Spread the threads to all CPUs and calculate
            // cycles from anchor separately
            let handles = cpuids.into_iter().map(|id| {
                std::thread::spawn(move || {
                    set_affinity(id).unwrap();

                    // check if cpu id matches IA32_TSC_AUX
                    let (_, cpuid) = tsc_with_cpuid();
                    assert_eq!(cpuid, id);

                    let (cps, cfa) = cycles_per_sec(anchor);

                    (id, cps, cfa)
                })
            });

            // Block and wait for all threads finished
            let results = handles.map(|h| h.join()).collect::<Result<Vec<_>, _>>();

            let results = if let Ok(results) = results {
                results
            } else {
                return TSCLevel::Unstable;
            };

            // Indexed by CPU ID
            let mut cycles_from_anchor = vec![0; max_cpu_id + 1];

            // Rates of TSCs on different CPUs won't be a big gap
            // or it's unstable.
            let mut max_cps = std::u64::MIN;
            let mut min_cps = std::u64::MAX;
            let mut sum_cps = 0;
            let len = results.len();
            for (cpuid, cps, cfa) in results {
                if cps > max_cps {
                    max_cps = cps;
                }
                if cps < min_cps {
                    min_cps = cps;
                }
                sum_cps += cps;
                cycles_from_anchor[cpuid] = cfa;
            }
            if (max_cps - min_cps) as f64 / min_cps as f64 > 0.0005 {
                return TSCLevel::Unstable;
            }

            return TSCLevel::PerCPUStable {
                cycles_per_second: sum_cps / len as u64,
                cycles_from_anchor,
            };
        }

        TSCLevel::Unstable
    }

    #[inline]
    fn cycles_per_second(&self) -> u64 {
        match self {
            TSCLevel::Stable {
                cycles_per_second, ..
            } => *cycles_per_second,
            TSCLevel::PerCPUStable {
                cycles_per_second, ..
            } => *cycles_per_second,
            TSCLevel::Unstable => panic!("tsc is unstable"),
        }
    }
}

/// If linux kernel detected TSCs are sync between CPUs, we can
/// rely on the result to say tsc is stable so that no need to
/// sync TSCs by ourselves.
fn is_tsc_stable() -> bool {
    let clock_source =
        read_to_string("/sys/devices/system/clocksource/clocksource0/available_clocksource");

    clock_source.map(|s| s.contains("tsc")).unwrap_or(false)
}

/// Checks if CPU flag contains `constant_tsc`, `nonstop_tsc` and
/// `rdtscp`. With these features, TSCs can be synced via offsets
/// between them and CPUID extracted from `IA32_TSC_AUX`.
fn is_tsc_percpu_stable() -> bool {
    let f = || {
        let cpuinfo = std::fs::File::open("/proc/cpuinfo").ok()?;
        let mut cpuinfo = BufReader::new(cpuinfo);

        let mut buf = String::with_capacity(1024);
        loop {
            if cpuinfo.read_line(&mut buf).ok()? == 0 {
                break;
            }

            if buf.starts_with("flags") {
                break;
            }

            buf.clear();
        }

        let constant_tsc = buf.contains("constant_tsc");
        let nonstop_tsc = buf.contains("nonstop_tsc");
        let rdtscp = buf.contains("rdtscp");
        Some(constant_tsc && nonstop_tsc && rdtscp)
    };

    f().unwrap_or(false)
}

#[derive(Debug)]
enum TscReadError {
    FailedToRead(std::io::Error),
    FailedToParse((core::num::ParseIntError, String)),
}

/// Attempts to read the TSC frequency as reported by the linux kernel.
/// This value only exists in Google's production kernel, but it is not upstreamed to the mainline kernel tree.
/// However it is possible to export the value using a custom kernel module:
/// https://github.com/trailofbits/tsc_freq_khz
fn try_read_tsc_freq_khz() -> Result<u64, TscReadError> {
    let s = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/tsc_freq_khz")
        .map_err(TscReadError::FailedToRead)?;
    s.trim()
        .parse()
        .map_err(|e| TscReadError::FailedToParse((e, s)))
}

/// Returns (1) cycles per second and (2) cycles from anchor.
/// The result of subtracting `cycles_from_anchor` from newly fetched TSC
/// can be used to
///   1. readjust TSC to begin from zero
///   2. sync TSCs between all CPUs
fn cycles_per_sec(anchor: Instant) -> (u64, u64) {
    let (cps, last_monotonic, last_tsc) = if let Ok(tsc_freq_khz) = try_read_tsc_freq_khz() {
        let (last_monotonic, last_tsc) = monotonic_with_tsc();
        (tsc_freq_khz * 1000, last_monotonic, last_tsc)
    } else {
        _calculate_cycles_per_sec()
    };

    let nanos_from_anchor = (last_monotonic - anchor).as_nanos();
    let cycles_flied = cps as f64 * nanos_from_anchor as f64 / 1_000_000_000.0;
    let cycles_from_anchor = last_tsc - cycles_flied.ceil() as u64;

    (cps, cycles_from_anchor)
}

/// Returns (1) cycles per second, (2) last monotonic time and (3) associated tsc.
fn _calculate_cycles_per_sec() -> (u64, Instant, u64) {
    let mut cycles_per_sec;
    let mut last_monotonic;
    let mut last_tsc;
    let mut old_cycles = 0.0;

    loop {
        let (t1, tsc1) = monotonic_with_tsc();
        loop {
            let (t2, tsc2) = monotonic_with_tsc();
            last_monotonic = t2;
            last_tsc = tsc2;
            let elapsed_nanos = (t2 - t1).as_nanos();
            if elapsed_nanos > 10_000_000 {
                cycles_per_sec = (tsc2 - tsc1) as f64 * 1_000_000_000.0 / elapsed_nanos as f64;
                break;
            }
        }
        let delta = f64::abs(cycles_per_sec - old_cycles);
        if delta / cycles_per_sec < 0.00001 {
            break;
        }
        old_cycles = cycles_per_sec;
    }

    (cycles_per_sec.round() as u64, last_monotonic, last_tsc)
}

/// Try to get tsc and monotonic time at the same time. Due to
/// get interrupted in half way may happen, they aren't guaranteed
/// to represent the same instant.
fn monotonic_with_tsc() -> (Instant, u64) {
    (Instant::now(), tsc())
}

#[inline]
fn tsc() -> u64 {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::_rdtsc;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::_rdtsc;

    unsafe { _rdtsc() }
}

#[inline]
fn tsc_with_cpuid() -> (u64, usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::__rdtscp;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::__rdtscp;

    let mut aux = MaybeUninit::<u32>::uninit();
    let tsc = unsafe { __rdtscp(aux.as_mut_ptr()) };
    let aux = unsafe { aux.assume_init() };

    // IA32_TSC_AUX are encoded by Linux kernel as follow format:
    //
    // 31       12 11      0
    // [ node id ][ cpu id ]
    //
    // See: https://elixir.bootlin.com/linux/v5.7.7/source/arch/x86/include/asm/segment.h#L249

    // extract cpu id and check the same
    (tsc, (aux & 0xfff) as usize)
}

// Retrieve available CPUs from `/sys` filesystem.
fn available_cpus() -> Result<Vec<usize>, Error> {
    let s = read_to_string("/sys/devices/system/cpu/online")?;
    parse_cpu_list_format(&s)
}

/// A wrapper function of sched_setaffinity(2)
fn set_affinity(cpuid: usize) -> Result<(), Error> {
    let mut set = unsafe { zeroed::<cpu_set_t>() };

    unsafe { CPU_SET(cpuid, &mut set) };

    // Set the current thread's core affinity.
    if unsafe {
        sched_setaffinity(
            0, // Defaults to current thread
            size_of::<cpu_set_t>(),
            &set as *const _,
        )
    } != 0
    {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

/// List format
/// The  List  Format for cpus and mems is a comma-separated list of CPU or
/// memory-node numbers and ranges of numbers, in ASCII decimal.
///
/// Examples of the List Format:
///   0-4,9           # bits 0, 1, 2, 3, 4, and 9 set
///   0-2,7,12-14     # bits 0, 1, 2, 7, 12, 13, and 14 set
fn parse_cpu_list_format(list: &str) -> Result<Vec<usize>, Error> {
    let mut res = vec![];
    let list = list.trim();
    for set in list.split(',') {
        if set.contains('-') {
            let mut ft = set.splitn(2, '-');
            let from = ft.next().ok_or("expected from")?.parse::<usize>()?;
            let to = ft.next().ok_or("expected to")?.parse::<usize>()?;
            for i in from..=to {
                res.push(i);
            }
        } else {
            res.push(set.parse()?)
        }
    }

    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_list_format() {
        assert_eq!(
            parse_cpu_list_format("0-2,7,12-14\n").unwrap(),
            &[0, 1, 2, 7, 12, 13, 14]
        );
        assert_eq!(
            parse_cpu_list_format("0-4,9\n").unwrap(),
            &[0, 1, 2, 3, 4, 9]
        );
        assert_eq!(
            parse_cpu_list_format("0-15\n").unwrap(),
            (0..=15).collect::<Vec<_>>()
        );
    }
}
