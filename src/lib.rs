//! A statistics-driven micro-benchmarking library written in Rust.
//!
//! This crate is a microbenchmarking library which aims to provide strong
//! statistical confidence in detecting and estimating the size of performance
//! improvements and regressions, while also being easy to use.
//!
//! See
//! [the user guide](https://bheisler.github.io/criterion.rs/book/index.html)
//! for examples as well as details on the measurement and analysis process,
//! and the output.
//!
//! ## Features:
//! * Collects detailed statistics, providing strong confidence that changes
//!   to performance are real, not measurement noise
//! * Produces detailed charts, providing thorough understanding of your code's
//!   performance behavior.

#![deny(missing_docs)]
#![deny(bare_trait_objects)]
#![deny(warnings)]
#![cfg_attr(feature = "real_blackbox", feature(test))]
#![cfg_attr(
    feature = "cargo-clippy",
    allow(
        clippy::just_underscores_and_digits, // Used in the stats code
        clippy::transmute_ptr_to_ptr, // Used in the stats code
        clippy::option_as_ref_deref, // Remove when MSRV bumped above 1.40
    )
)]

#[cfg(test)]
extern crate approx;

#[cfg(test)]
extern crate quickcheck;

use clap::value_t;
use regex::Regex;

#[macro_use]
extern crate lazy_static;

#[cfg(feature = "real_blackbox")]
extern crate test;

#[macro_use]
extern crate serde_derive;

// Needs to be declared before other modules
// in order to be usable there.
#[macro_use]
mod macros_private;
#[macro_use]
mod analysis;
mod benchmark;
#[macro_use]
mod benchmark_group;
mod connection;
mod csv_report;
mod error;
mod estimate;
mod format;
mod fs;
mod html;
mod kde;
mod macros;
pub mod measurement;
mod plot;
pub mod profiler;
mod report;
mod routine;
mod stats;

use std::cell::RefCell;
use std::collections::HashSet;
use std::default::Default;
use std::fmt;
use std::iter::IntoIterator;
use std::marker::PhantomData;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use std::time::Instant;

use criterion_plot::{Version, VersionError};

use crate::benchmark::BenchmarkConfig;
use crate::benchmark::NamedRoutine;
use crate::connection::Connection;
use crate::connection::OutgoingMessage;
use crate::csv_report::FileCsvReport;
use crate::html::Html;
use crate::measurement::{Measurement, WallTime};
use crate::plot::{Gnuplot, Plotter, PlottersBackend};
use crate::profiler::{ExternalProfiler, Profiler};
use crate::report::{BencherReport, CliReport, Report, ReportContext, Reports};
use crate::routine::Function;

pub use crate::benchmark::{Benchmark, BenchmarkDefinition, ParameterizedBenchmark};
pub use crate::benchmark_group::{BenchmarkGroup, BenchmarkId};

lazy_static! {
    static ref DEBUG_ENABLED: bool = std::env::var_os("CRITERION_DEBUG").is_some();
    static ref GNUPLOT_VERSION: Result<Version, VersionError> = criterion_plot::version();
    static ref DEFAULT_PLOTTING_BACKEND: PlottingBackend = {
        match &*GNUPLOT_VERSION {
            Ok(_) => PlottingBackend::Gnuplot,
            Err(e) => {
                match e {
                    VersionError::Exec(_) => println!("Gnuplot not found, using plotters backend"),
                    e => println!(
                        "Gnuplot not found or not usable, using plotters backend\n{}",
                        e
                    ),
                };
                PlottingBackend::Plotters
            }
        }
    };
    static ref CARGO_CRITERION_CONNECTION: Option<Mutex<Connection>> = {
        match std::env::var("CARGO_CRITERION_PORT") {
            Ok(port_str) => {
                let port: u16 = port_str.parse().ok()?;
                let stream = TcpStream::connect(("localhost", port)).ok()?;
                Some(Mutex::new(Connection::new(stream).ok()?))
            }
            Err(_) => None,
        }
    };
}

fn debug_enabled() -> bool {
    *DEBUG_ENABLED
}

/// A function that is opaque to the optimizer, used to prevent the compiler from
/// optimizing away computations in a benchmark.
///
/// This variant is backed by the (unstable) test::black_box function.
#[cfg(feature = "real_blackbox")]
pub fn black_box<T>(dummy: T) -> T {
    test::black_box(dummy)
}

/// A function that is opaque to the optimizer, used to prevent the compiler from
/// optimizing away computations in a benchmark.
///
/// This variant is stable-compatible, but it may cause some performance overhead
/// or fail to prevent code from being eliminated.
#[cfg(not(feature = "real_blackbox"))]
pub fn black_box<T>(dummy: T) -> T {
    unsafe {
        let ret = std::ptr::read_volatile(&dummy);
        std::mem::forget(dummy);
        ret
    }
}

/// Representing a function to benchmark together with a name of that function.
/// Used together with `bench_functions` to represent one out of multiple functions
/// under benchmark.
#[doc(hidden)]
pub struct Fun<I: fmt::Debug, M: Measurement + 'static = WallTime> {
    f: NamedRoutine<I, M>,
    _phantom: PhantomData<M>,
}

impl<I, M: Measurement> Fun<I, M>
where
    I: fmt::Debug + 'static,
{
    /// Create a new `Fun` given a name and a closure
    pub fn new<F>(name: &str, f: F) -> Fun<I, M>
    where
        F: FnMut(&mut Bencher<'_, M>, &I) + 'static,
    {
        let routine = NamedRoutine {
            id: name.to_owned(),
            f: Box::new(RefCell::new(Function::new(f))),
        };

        Fun {
            f: routine,
            _phantom: PhantomData,
        }
    }
}

/// Argument to [`Bencher::iter_batched`](struct.Bencher.html#method.iter_batched) and
/// [`Bencher::iter_batched_ref`](struct.Bencher.html#method.iter_batched_ref) which controls the
/// batch size.
///
/// Generally speaking, almost all benchmarks should use `SmallInput`. If the input or the result
/// of the benchmark routine is large enough that `SmallInput` causes out-of-memory errors,
/// `LargeInput` can be used to reduce memory usage at the cost of increasing the measurement
/// overhead. If the input or the result is extremely large (or if it holds some
/// limited external resource like a file handle), `PerIteration` will set the number of iterations
/// per batch to exactly one. `PerIteration` can increase the measurement overhead substantially
/// and should be avoided wherever possible.
///
/// Each value lists an estimate of the measurement overhead. This is intended as a rough guide
/// to assist in choosing an option, it should not be relied upon. In particular, it is not valid
/// to subtract the listed overhead from the measurement and assume that the result represents the
/// true runtime of a function. The actual measurement overhead for your specific benchmark depends
/// on the details of the function you're benchmarking and the hardware and operating
/// system running the benchmark.
///
/// With that said, if the runtime of your function is small relative to the measurement overhead
/// it will be difficult to take accurate measurements. In this situation, the best option is to use
/// [`Bencher::iter`](struct.Bencher.html#method.iter) which has next-to-zero measurement overhead.
#[derive(Debug, Eq, PartialEq, Copy, Hash, Clone)]
pub enum BatchSize {
    /// `SmallInput` indicates that the input to the benchmark routine (the value returned from
    /// the setup routine) is small enough that millions of values can be safely held in memory.
    /// Always prefer `SmallInput` unless the benchmark is using too much memory.
    ///
    /// In testing, the maximum measurement overhead from benchmarking with `SmallInput` is on the
    /// order of 500 picoseconds. This is presented as a rough guide; your results may vary.
    SmallInput,

    /// `LargeInput` indicates that the input to the benchmark routine or the value returned from
    /// that routine is large. This will reduce the memory usage but increase the measurement
    /// overhead.
    ///
    /// In testing, the maximum measurement overhead from benchmarking with `LargeInput` is on the
    /// order of 750 picoseconds. This is presented as a rough guide; your results may vary.
    LargeInput,

    /// `PerIteration` indicates that the input to the benchmark routine or the value returned from
    /// that routine is extremely large or holds some limited resource, such that holding many values
    /// in memory at once is infeasible. This provides the worst measurement overhead, but the
    /// lowest memory usage.
    ///
    /// In testing, the maximum measurement overhead from benchmarking with `PerIteration` is on the
    /// order of 350 nanoseconds or 350,000 picoseconds. This is presented as a rough guide; your
    /// results may vary.
    PerIteration,

    /// `NumBatches` will attempt to divide the iterations up into a given number of batches.
    /// A larger number of batches (and thus smaller batches) will reduce memory usage but increase
    /// measurement overhead. This allows the user to choose their own tradeoff between memory usage
    /// and measurement overhead, but care must be taken in tuning the number of batches. Most
    /// benchmarks should use `SmallInput` or `LargeInput` instead.
    NumBatches(u64),

    /// `NumIterations` fixes the batch size to a constant number, specified by the user. This
    /// allows the user to choose their own tradeoff between overhead and memory usage, but care must
    /// be taken in tuning the batch size. In general, the measurement overhead of NumIterations
    /// will be larger than that of `NumBatches`. Most benchmarks should use `SmallInput` or
    /// `LargeInput` instead.
    NumIterations(u64),

    #[doc(hidden)]
    __NonExhaustive,
}
impl BatchSize {
    /// Convert to a number of iterations per batch.
    ///
    /// We try to do a constant number of batches regardless of the number of iterations in this
    /// sample. If the measurement overhead is roughly constant regardless of the number of
    /// iterations the analysis of the results later will have an easier time separating the
    /// measurement overhead from the benchmark time.
    fn iters_per_batch(self, iters: u64) -> u64 {
        match self {
            BatchSize::SmallInput => (iters + 10 - 1) / 10,
            BatchSize::LargeInput => (iters + 1000 - 1) / 1000,
            BatchSize::PerIteration => 1,
            BatchSize::NumBatches(batches) => (iters + batches - 1) / batches,
            BatchSize::NumIterations(size) => size,
            BatchSize::__NonExhaustive => panic!("__NonExhaustive is not a valid BatchSize."),
        }
    }
}

/// Timer struct used to iterate a benchmarked function and measure the runtime.
///
/// This struct provides different timing loops as methods. Each timing loop provides a different
/// way to time a routine and each has advantages and disadvantages.
///
/// * If you want to do the iteration and measurement yourself (eg. passing the iteration count
///   to a separate process), use `iter_custom`.
/// * If your routine requires no per-iteration setup and returns a value with an expensive `drop`
///   method, use `iter_with_large_drop`.
/// * If your routine requires some per-iteration setup that shouldn't be timed, use `iter_batched`
///   or `iter_batched_ref`. See [`BatchSize`](enum.BatchSize.html) for a discussion of batch sizes.
///   If the setup value implements `Drop` and you don't want to include the `drop` time in the
///   measurement, use `iter_batched_ref`, otherwise use `iter_batched`. These methods are also
///   suitable for benchmarking routines which return a value with an expensive `drop` method,
///   but are more complex than `iter_with_large_drop`.
/// * Otherwise, use `iter`.
pub struct Bencher<'a, M: Measurement = WallTime> {
    iterated: bool,         // have we iterated this benchmark?
    iters: u64,             // Number of times to iterate this benchmark
    value: M::Value,        // The measured value
    measurement: &'a M,     // Reference to the measurement object
    elapsed_time: Duration, // How much time did it take to perform the iteration? Used for the warmup period.
}
impl<'a, M: Measurement> Bencher<'a, M> {
    /// Times a `routine` by executing it many times and timing the total elapsed time.
    ///
    /// Prefer this timing loop when `routine` returns a value that doesn't have a destructor.
    ///
    /// # Timing model
    ///
    /// Note that the `Bencher` also times the time required to destroy the output of `routine()`.
    /// Therefore prefer this timing loop when the runtime of `mem::drop(O)` is negligible compared
    /// to the runtime of the `routine`.
    ///
    /// ```text
    /// elapsed = Instant::now + iters * (routine + mem::drop(O) + Range::next)
    /// ```
    ///
    /// # Example
    ///
    /// ```rust
    /// #[macro_use] extern crate criterion;
    ///
    /// use criterion::*;
    ///
    /// // The function to benchmark
    /// fn foo() {
    ///     // ...
    /// }
    ///
    /// fn bench(c: &mut Criterion) {
    ///     c.bench_function("iter", move |b| {
    ///         b.iter(|| foo())
    ///     });
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    ///
    #[inline(never)]
    pub fn iter<O, R>(&mut self, mut routine: R)
    where
        R: FnMut() -> O,
    {
        self.iterated = true;
        let time_start = Instant::now();
        let start = self.measurement.start();
        for _ in 0..self.iters {
            black_box(routine());
        }
        self.value = self.measurement.end(start);
        self.elapsed_time = time_start.elapsed();
    }

    /// Times a `routine` by executing it many times and relying on `routine` to measure its own execution time.
    ///
    /// Prefer this timing loop in cases where `routine` has to do its own measurements to
    /// get accurate timing information (for example in multi-threaded scenarios where you spawn
    /// and coordinate with multiple threads).
    ///
    /// # Timing model
    /// Custom, the timing model is whatever is returned as the Duration from `routine`.
    ///
    /// # Example
    /// ```rust
    /// #[macro_use] extern crate criterion;
    /// use criterion::*;
    /// use criterion::black_box;
    /// use std::time::Instant;
    ///
    /// fn foo() {
    ///     // ...
    /// }
    ///
    /// fn bench(c: &mut Criterion) {
    ///     c.bench_function("iter", move |b| {
    ///         b.iter_custom(|iters| {
    ///             let start = Instant::now();
    ///             for _i in 0..iters {
    ///                 black_box(foo());
    ///             }
    ///             start.elapsed()
    ///         })
    ///     });
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    ///
    #[inline(never)]
    pub fn iter_custom<R>(&mut self, mut routine: R)
    where
        R: FnMut(u64) -> M::Value,
    {
        self.iterated = true;
        let time_start = Instant::now();
        self.value = routine(self.iters);
        self.elapsed_time = time_start.elapsed();
    }

    #[doc(hidden)]
    pub fn iter_with_setup<I, O, S, R>(&mut self, setup: S, routine: R)
    where
        S: FnMut() -> I,
        R: FnMut(I) -> O,
    {
        self.iter_batched(setup, routine, BatchSize::PerIteration);
    }

    /// Times a `routine` by collecting its output on each iteration. This avoids timing the
    /// destructor of the value returned by `routine`.
    ///
    /// WARNING: This requires `O(iters * mem::size_of::<O>())` of memory, and `iters` is not under the
    /// control of the caller. If this causes out-of-memory errors, use `iter_batched` instead.
    ///
    /// # Timing model
    ///
    /// ``` text
    /// elapsed = Instant::now + iters * (routine) + Iterator::collect::<Vec<_>>
    /// ```
    ///
    /// # Example
    ///
    /// ```rust
    /// #[macro_use] extern crate criterion;
    ///
    /// use criterion::*;
    ///
    /// fn create_vector() -> Vec<u64> {
    ///     # vec![]
    ///     // ...
    /// }
    ///
    /// fn bench(c: &mut Criterion) {
    ///     c.bench_function("with_drop", move |b| {
    ///         // This will avoid timing the Vec::drop.
    ///         b.iter_with_large_drop(|| create_vector())
    ///     });
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    ///
    pub fn iter_with_large_drop<O, R>(&mut self, mut routine: R)
    where
        R: FnMut() -> O,
    {
        self.iter_batched(|| (), |_| routine(), BatchSize::SmallInput);
    }

    #[doc(hidden)]
    pub fn iter_with_large_setup<I, O, S, R>(&mut self, setup: S, routine: R)
    where
        S: FnMut() -> I,
        R: FnMut(I) -> O,
    {
        self.iter_batched(setup, routine, BatchSize::NumBatches(1));
    }

    /// Times a `routine` that requires some input by generating a batch of input, then timing the
    /// iteration of the benchmark over the input. See [`BatchSize`](enum.BatchSize.html) for
    /// details on choosing the batch size. Use this when the routine must consume its input.
    ///
    /// For example, use this loop to benchmark sorting algorithms, because they require unsorted
    /// data on each iteration.
    ///
    /// # Timing model
    ///
    /// ```text
    /// elapsed = (Instant::now * num_batches) + (iters * (routine + O::drop)) + Vec::extend
    /// ```
    ///
    /// # Example
    ///
    /// ```rust
    /// #[macro_use] extern crate criterion;
    ///
    /// use criterion::*;
    ///
    /// fn create_scrambled_data() -> Vec<u64> {
    ///     # vec![]
    ///     // ...
    /// }
    ///
    /// // The sorting algorithm to test
    /// fn sort(data: &mut [u64]) {
    ///     // ...
    /// }
    ///
    /// fn bench(c: &mut Criterion) {
    ///     let data = create_scrambled_data();
    ///
    ///     c.bench_function("with_setup", move |b| {
    ///         // This will avoid timing the to_vec call.
    ///         b.iter_batched(|| data.clone(), |mut data| sort(&mut data), BatchSize::SmallInput)
    ///     });
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    ///
    #[inline(never)]
    pub fn iter_batched<I, O, S, R>(&mut self, mut setup: S, mut routine: R, size: BatchSize)
    where
        S: FnMut() -> I,
        R: FnMut(I) -> O,
    {
        self.iterated = true;
        let batch_size = size.iters_per_batch(self.iters);
        assert!(batch_size != 0, "Batch size must not be zero.");
        let time_start = Instant::now();
        self.value = self.measurement.zero();

        if batch_size == 1 {
            for _ in 0..self.iters {
                let input = black_box(setup());

                let start = self.measurement.start();
                let output = routine(input);
                let end = self.measurement.end(start);
                self.value = self.measurement.add(&self.value, &end);

                drop(black_box(output));
            }
        } else {
            let mut iteration_counter = 0;

            while iteration_counter < self.iters {
                let batch_size = ::std::cmp::min(batch_size, self.iters - iteration_counter);

                let inputs = black_box((0..batch_size).map(|_| setup()).collect::<Vec<_>>());
                let mut outputs = Vec::with_capacity(batch_size as usize);

                let start = self.measurement.start();
                outputs.extend(inputs.into_iter().map(&mut routine));
                let end = self.measurement.end(start);
                self.value = self.measurement.add(&self.value, &end);

                black_box(outputs);

                iteration_counter += batch_size;
            }
        }

        self.elapsed_time = time_start.elapsed();
    }

    /// Times a `routine` that requires some input by generating a batch of input, then timing the
    /// iteration of the benchmark over the input. See [`BatchSize`](enum.BatchSize.html) for
    /// details on choosing the batch size. Use this when the routine should accept the input by
    /// mutable reference.
    ///
    /// For example, use this loop to benchmark sorting algorithms, because they require unsorted
    /// data on each iteration.
    ///
    /// # Timing model
    ///
    /// ```text
    /// elapsed = (Instant::now * num_batches) + (iters * routine) + Vec::extend
    /// ```
    ///
    /// # Example
    ///
    /// ```rust
    /// #[macro_use] extern crate criterion;
    ///
    /// use criterion::*;
    ///
    /// fn create_scrambled_data() -> Vec<u64> {
    ///     # vec![]
    ///     // ...
    /// }
    ///
    /// // The sorting algorithm to test
    /// fn sort(data: &mut [u64]) {
    ///     // ...
    /// }
    ///
    /// fn bench(c: &mut Criterion) {
    ///     let data = create_scrambled_data();
    ///
    ///     c.bench_function("with_setup", move |b| {
    ///         // This will avoid timing the to_vec call.
    ///         b.iter_batched(|| data.clone(), |mut data| sort(&mut data), BatchSize::SmallInput)
    ///     });
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    ///
    #[inline(never)]
    pub fn iter_batched_ref<I, O, S, R>(&mut self, mut setup: S, mut routine: R, size: BatchSize)
    where
        S: FnMut() -> I,
        R: FnMut(&mut I) -> O,
    {
        self.iterated = true;
        let batch_size = size.iters_per_batch(self.iters);
        assert!(batch_size != 0, "Batch size must not be zero.");
        let time_start = Instant::now();
        self.value = self.measurement.zero();

        if batch_size == 1 {
            for _ in 0..self.iters {
                let mut input = black_box(setup());

                let start = self.measurement.start();
                let output = routine(&mut input);
                let end = self.measurement.end(start);
                self.value = self.measurement.add(&self.value, &end);

                drop(black_box(output));
                drop(black_box(input));
            }
        } else {
            let mut iteration_counter = 0;

            while iteration_counter < self.iters {
                let batch_size = ::std::cmp::min(batch_size, self.iters - iteration_counter);

                let mut inputs = black_box((0..batch_size).map(|_| setup()).collect::<Vec<_>>());
                let mut outputs = Vec::with_capacity(batch_size as usize);

                let start = self.measurement.start();
                outputs.extend(inputs.iter_mut().map(&mut routine));
                let end = self.measurement.end(start);
                self.value = self.measurement.add(&self.value, &end);

                black_box(outputs);

                iteration_counter += batch_size;
            }
        }
        self.elapsed_time = time_start.elapsed();
    }

    // Benchmarks must actually call one of the iter methods. This causes benchmarks to fail loudly
    // if they don't.
    fn assert_iterated(&mut self) {
        if !self.iterated {
            panic!("Benchmark function must call Bencher::iter or related method.");
        }
        self.iterated = false;
    }
}

/// Baseline describes how the baseline_directory is handled.
#[derive(Debug, Clone, Copy)]
pub enum Baseline {
    /// Compare ensures a previous saved version of the baseline
    /// exists and runs comparison against that.
    Compare,
    /// Save writes the benchmark results to the baseline directory,
    /// overwriting any results that were previously there.
    Save,
}

/// Enum used to select the plotting backend.
#[derive(Debug, Clone, Copy)]
pub enum PlottingBackend {
    /// Plotting backend which uses the external `gnuplot` command to render plots. This is the
    /// default if the `gnuplot` command is installed.
    Gnuplot,
    /// Plotting backend which uses the rust 'Plotters' library. This is the default if `gnuplot`
    /// is not installed.
    Plotters,
}

#[derive(Debug, Clone)]
/// Enum representing the execution mode.
pub(crate) enum Mode {
    /// Run benchmarks normally
    Benchmark,
    /// List all benchmarks but do not run them.
    List,
    /// Run bennchmarks once to verify that they work, but otherwise do not measure them.
    Test,
    /// Iterate benchmarks for a given length of time but do not analyze or report on them.
    Profile(Duration),
}
impl Mode {
    pub fn is_benchmark(&self) -> bool {
        match self {
            Mode::Benchmark => true,
            _ => false,
        }
    }
}

/// The benchmark manager
///
/// `Criterion` lets you configure and execute benchmarks
///
/// Each benchmark consists of four phases:
///
/// - **Warm-up**: The routine is repeatedly executed, to let the CPU/OS/JIT/interpreter adapt to
/// the new load
/// - **Measurement**: The routine is repeatedly executed, and timing information is collected into
/// a sample
/// - **Analysis**: The sample is analyzed and distiled into meaningful statistics that get
/// reported to stdout, stored in files, and plotted
/// - **Comparison**: The current sample is compared with the sample obtained in the previous
/// benchmark.
pub struct Criterion<M: Measurement = WallTime> {
    config: BenchmarkConfig,
    plotting_backend: PlottingBackend,
    plotting_enabled: bool,
    filter: Option<Regex>,
    report: Box<dyn Report>,
    output_directory: PathBuf,
    baseline_directory: String,
    baseline: Baseline,
    load_baseline: Option<String>,
    all_directories: HashSet<String>,
    all_titles: HashSet<String>,
    measurement: M,
    profiler: Box<RefCell<dyn Profiler>>,
    connection: Option<MutexGuard<'static, Connection>>,
    mode: Mode,
}

impl Default for Criterion {
    /// Creates a benchmark manager with the following default settings:
    ///
    /// - Sample size: 100 measurements
    /// - Warm-up time: 3 s
    /// - Measurement time: 5 s
    /// - Bootstrap size: 100 000 resamples
    /// - Noise threshold: 0.01 (1%)
    /// - Confidence level: 0.95
    /// - Significance level: 0.05
    /// - Plotting: enabled, using gnuplot if available or plotters if gnuplot is not available
    /// - No filter
    fn default() -> Criterion {
        let mut reports: Vec<Box<dyn Report>> = vec![];
        if CARGO_CRITERION_CONNECTION.is_none() {
            reports.push(Box::new(CliReport::new(false, false, false)));
        }
        reports.push(Box::new(FileCsvReport));

        // Set criterion home to (in descending order of preference):
        // - $CRITERION_HOME (cargo-criterion sets this, but other users could as well)
        // - $CARGO_TARGET_DIR/criterion
        // - ./target/criterion
        let output_directory = if let Some(value) = std::env::var_os("CRITERION_HOME") {
            PathBuf::from(value)
        } else if let Some(value) = std::env::var_os("CARGO_TARGET_DIR") {
            PathBuf::from(value).join("criterion")
        } else {
            PathBuf::from("target/criterion")
        };

        Criterion {
            config: BenchmarkConfig {
                confidence_level: 0.95,
                measurement_time: Duration::new(5, 0),
                noise_threshold: 0.01,
                nresamples: 100_000,
                sample_size: 100,
                significance_level: 0.05,
                warm_up_time: Duration::new(3, 0),
            },
            plotting_backend: *DEFAULT_PLOTTING_BACKEND,
            plotting_enabled: true,
            filter: None,
            report: Box::new(Reports::new(reports)),
            baseline_directory: "base".to_owned(),
            baseline: Baseline::Save,
            load_baseline: None,
            output_directory,
            all_directories: HashSet::new(),
            all_titles: HashSet::new(),
            measurement: WallTime,
            profiler: Box::new(RefCell::new(ExternalProfiler)),
            connection: CARGO_CRITERION_CONNECTION
                .as_ref()
                .map(|mtx| mtx.lock().unwrap()),
            mode: Mode::Benchmark,
        }
    }
}

impl<M: Measurement> Criterion<M> {
    /// Changes the measurement for the benchmarks run with this runner. See the
    /// Measurement trait for more details
    pub fn with_measurement<M2: Measurement>(self, m: M2) -> Criterion<M2> {
        // Can't use struct update syntax here because they're technically different types.
        Criterion {
            config: self.config,
            plotting_backend: self.plotting_backend,
            plotting_enabled: self.plotting_enabled,
            filter: self.filter,
            report: self.report,
            baseline_directory: self.baseline_directory,
            baseline: self.baseline,
            load_baseline: self.load_baseline,
            output_directory: self.output_directory,
            all_directories: self.all_directories,
            all_titles: self.all_titles,
            measurement: m,
            profiler: self.profiler,
            connection: self.connection,
            mode: self.mode,
        }
    }

    /// Changes the internal profiler for benchmarks run with this runner. See
    /// the Profiler trait for more details.
    pub fn with_profiler<P: Profiler + 'static>(self, p: P) -> Criterion<M> {
        Criterion {
            profiler: Box::new(RefCell::new(p)),
            ..self
        }
    }

    /// Set the plotting backend. By default, Criterion will use gnuplot if available, or plotters
    /// if not.
    ///
    /// Panics if `backend` is `PlottingBackend::Gnuplot` and gnuplot is not available.
    pub fn plotting_backend(self, backend: PlottingBackend) -> Criterion<M> {
        if let PlottingBackend::Gnuplot = backend {
            if GNUPLOT_VERSION.is_err() {
                panic!("Gnuplot plotting backend was requested, but gnuplot is not available. To continue, either install Gnuplot or allow Criterion.rs to fall back to using plotters.");
            }
        }

        Criterion {
            plotting_backend: backend,
            ..self
        }
    }

    /// Changes the default size of the sample for benchmarks run with this runner.
    ///
    /// A bigger sample should yield more accurate results if paired with a sufficiently large
    /// measurement time.
    ///
    /// Sample size must be at least 10.
    ///
    /// # Panics
    ///
    /// Panics if n < 10
    pub fn sample_size(mut self, n: usize) -> Criterion<M> {
        assert!(n >= 10);

        self.config.sample_size = n;
        self
    }

    /// Changes the default warm up time for benchmarks run with this runner.
    ///
    /// # Panics
    ///
    /// Panics if the input duration is zero
    pub fn warm_up_time(mut self, dur: Duration) -> Criterion<M> {
        assert!(dur.to_nanos() > 0);

        self.config.warm_up_time = dur;
        self
    }

    /// Changes the default measurement time for benchmarks run with this runner.
    ///
    /// With a longer time, the measurement will become more resilient to transitory peak loads
    /// caused by external programs
    ///
    /// **Note**: If the measurement time is too "low", Criterion will automatically increase it
    ///
    /// # Panics
    ///
    /// Panics if the input duration in zero
    pub fn measurement_time(mut self, dur: Duration) -> Criterion<M> {
        assert!(dur.to_nanos() > 0);

        self.config.measurement_time = dur;
        self
    }

    /// Changes the default number of resamples for benchmarks run with this runner.
    ///
    /// Number of resamples to use for the
    /// [bootstrap](http://en.wikipedia.org/wiki/Bootstrapping_(statistics)#Case_resampling)
    ///
    /// A larger number of resamples reduces the random sampling errors, which are inherent to the
    /// bootstrap method, but also increases the analysis time
    ///
    /// # Panics
    ///
    /// Panics if the number of resamples is set to zero
    pub fn nresamples(mut self, n: usize) -> Criterion<M> {
        assert!(n > 0);
        if n <= 1000 {
            println!("\nWarning: It is not recommended to reduce nresamples below 1000.");
        }

        self.config.nresamples = n;
        self
    }

    /// Changes the default noise threshold for benchmarks run with this runner. The noise threshold
    /// is used to filter out small changes in performance, even if they are statistically
    /// significant. Sometimes benchmarking the same code twice will result in small but
    /// statistically significant differences solely because of noise. This provides a way to filter
    /// out some of these false positives at the cost of making it harder to detect small changes
    /// to the true performance of the benchmark.
    ///
    /// The default is 0.01, meaning that changes smaller than 1% will be ignored.
    ///
    /// # Panics
    ///
    /// Panics if the threshold is set to a negative value
    pub fn noise_threshold(mut self, threshold: f64) -> Criterion<M> {
        assert!(threshold >= 0.0);

        self.config.noise_threshold = threshold;
        self
    }

    /// Changes the default confidence level for benchmarks run with this runner. The confidence
    /// level is the desired probability that the true runtime lies within the estimated
    /// [confidence interval](https://en.wikipedia.org/wiki/Confidence_interval). The default is
    /// 0.95, meaning that the confidence interval should capture the true value 95% of the time.
    ///
    /// # Panics
    ///
    /// Panics if the confidence level is set to a value outside the `(0, 1)` range
    pub fn confidence_level(mut self, cl: f64) -> Criterion<M> {
        assert!(cl > 0.0 && cl < 1.0);
        if cl < 0.5 {
            println!("\nWarning: It is not recommended to reduce confidence level below 0.5.");
        }

        self.config.confidence_level = cl;
        self
    }

    /// Changes the default [significance level](https://en.wikipedia.org/wiki/Statistical_significance)
    /// for benchmarks run with this runner. This is used to perform a
    /// [hypothesis test](https://en.wikipedia.org/wiki/Statistical_hypothesis_testing) to see if
    /// the measurements from this run are different from the measured performance of the last run.
    /// The significance level is the desired probability that two measurements of identical code
    /// will be considered 'different' due to noise in the measurements. The default value is 0.05,
    /// meaning that approximately 5% of identical benchmarks will register as different due to
    /// noise.
    ///
    /// This presents a trade-off. By setting the significance level closer to 0.0, you can increase
    /// the statistical robustness against noise, but it also weaken's Criterion.rs' ability to
    /// detect small but real changes in the performance. By setting the significance level
    /// closer to 1.0, Criterion.rs will be more able to detect small true changes, but will also
    /// report more spurious differences.
    ///
    /// See also the noise threshold setting.
    ///
    /// # Panics
    ///
    /// Panics if the significance level is set to a value outside the `(0, 1)` range
    pub fn significance_level(mut self, sl: f64) -> Criterion<M> {
        assert!(sl > 0.0 && sl < 1.0);

        self.config.significance_level = sl;
        self
    }

    fn create_plotter(&self) -> Box<dyn Plotter> {
        match self.plotting_backend {
            PlottingBackend::Gnuplot => Box::new(Gnuplot::default()),
            PlottingBackend::Plotters => Box::new(PlottersBackend::default()),
        }
    }

    /// Enables plotting
    pub fn with_plots(mut self) -> Criterion<M> {
        self.plotting_enabled = true;
        let mut reports: Vec<Box<dyn Report>> = vec![];
        if self.connection.is_none() {
            reports.push(Box::new(CliReport::new(false, false, false)));
        }
        reports.push(Box::new(FileCsvReport));
        reports.push(Box::new(Html::new(self.create_plotter())));
        self.report = Box::new(Reports::new(reports));

        self
    }

    /// Disables plotting
    pub fn without_plots(mut self) -> Criterion<M> {
        self.plotting_enabled = false;
        let mut reports: Vec<Box<dyn Report>> = vec![];
        if self.connection.is_none() {
            reports.push(Box::new(CliReport::new(false, false, false)));
        }
        reports.push(Box::new(FileCsvReport));
        self.report = Box::new(Reports::new(reports));
        self
    }

    /// Return true if generation of the plots is possible.
    pub fn can_plot(&self) -> bool {
        // Trivially true now that we have plotters.
        // TODO: Deprecate and remove this.
        true
    }

    /// Names an explicit baseline and enables overwriting the previous results.
    pub fn save_baseline(mut self, baseline: String) -> Criterion<M> {
        self.baseline_directory = baseline;
        self.baseline = Baseline::Save;
        self
    }

    /// Names an explicit baseline and disables overwriting the previous results.
    pub fn retain_baseline(mut self, baseline: String) -> Criterion<M> {
        self.baseline_directory = baseline;
        self.baseline = Baseline::Compare;
        self
    }

    /// Filters the benchmarks. Only benchmarks with names that contain the
    /// given string will be executed.
    pub fn with_filter<S: Into<String>>(mut self, filter: S) -> Criterion<M> {
        let filter_text = filter.into();
        let filter = Regex::new(&filter_text).unwrap_or_else(|err| {
            panic!(
                "Unable to parse '{}' as a regular expression: {}",
                filter_text, err
            )
        });
        self.filter = Some(filter);

        self
    }

    /// Set the output directory (currently for testing only)
    #[doc(hidden)]
    pub fn output_directory(mut self, path: &Path) -> Criterion<M> {
        self.output_directory = path.to_owned();

        self
    }

    /// Set the profile time (currently for testing only)
    #[doc(hidden)]
    pub fn profile_time(mut self, profile_time: Option<Duration>) -> Criterion<M> {
        match profile_time {
            Some(time) => self.mode = Mode::Profile(time),
            None => self.mode = Mode::Benchmark,
        }

        self
    }

    /// Generate the final summary at the end of a run.
    #[doc(hidden)]
    pub fn final_summary(&self) {
        if !self.mode.is_benchmark() {
            return;
        }

        let report_context = ReportContext {
            output_directory: self.output_directory.clone(),
            plot_config: PlotConfiguration::default(),
        };

        self.report.final_summary(&report_context);
    }

    /// Configure this criterion struct based on the command-line arguments to
    /// this process.
    #[cfg_attr(feature = "cargo-clippy", allow(clippy::cognitive_complexity))]
    pub fn configure_from_args(mut self) -> Criterion<M> {
        use clap::{App, Arg};
        let matches = App::new("Criterion Benchmark")
            .arg(Arg::with_name("FILTER")
                .help("Skip benchmarks whose names do not contain FILTER.")
                .index(1))
            .arg(Arg::with_name("color")
                .short("c")
                .long("color")
                .alias("colour")
                .takes_value(true)
                .possible_values(&["auto", "always", "never"])
                .default_value("auto")
                .help("Configure coloring of output. always = always colorize output, never = never colorize output, auto = colorize output if output is a tty and compiled for unix."))
            .arg(Arg::with_name("verbose")
                .short("v")
                .long("verbose")
                .help("Print additional statistical information."))
            .arg(Arg::with_name("noplot")
                .short("n")
                .long("noplot")
                .help("Disable plot and HTML generation."))
            .arg(Arg::with_name("save-baseline")
                .short("s")
                .long("save-baseline")
                .default_value("base")
                .help("Save results under a named baseline."))
            .arg(Arg::with_name("baseline")
                .short("b")
                .long("baseline")
                .takes_value(true)
                .conflicts_with("save-baseline")
                .help("Compare to a named baseline."))
            .arg(Arg::with_name("list")
                .long("list")
                .help("List all benchmarks")
                .conflicts_with_all(&["test", "profile-time"]))
            .arg(Arg::with_name("profile-time")
                .long("profile-time")
                .takes_value(true)
                .help("Iterate each benchmark for approximately the given number of seconds, doing no analysis and without storing the results. Useful for running the benchmarks in a profiler.")
                .conflicts_with_all(&["test", "list"]))
            .arg(Arg::with_name("load-baseline")
                 .long("load-baseline")
                 .takes_value(true)
                 .conflicts_with("profile-time")
                 .requires("baseline")
                 .help("Load a previous baseline instead of sampling new data."))
            .arg(Arg::with_name("sample-size")
                .long("sample-size")
                .takes_value(true)
                .help(&format!("Changes the default size of the sample for this run. [default: {}]", self.config.sample_size)))
            .arg(Arg::with_name("warm-up-time")
                .long("warm-up-time")
                .takes_value(true)
                .help(&format!("Changes the default warm up time for this run. [default: {}]", self.config.warm_up_time.as_secs())))
            .arg(Arg::with_name("measurement-time")
                .long("measurement-time")
                .takes_value(true)
                .help(&format!("Changes the default measurement time for this run. [default: {}]", self.config.measurement_time.as_secs())))
            .arg(Arg::with_name("nresamples")
                .long("nresamples")
                .takes_value(true)
                .help(&format!("Changes the default number of resamples for this run. [default: {}]", self.config.nresamples)))
            .arg(Arg::with_name("noise-threshold")
                .long("noise-threshold")
                .takes_value(true)
                .help(&format!("Changes the default noise threshold for this run. [default: {}]", self.config.noise_threshold)))
            .arg(Arg::with_name("confidence-level")
                .long("confidence-level")
                .takes_value(true)
                .help(&format!("Changes the default confidence level for this run. [default: {}]", self.config.confidence_level)))
            .arg(Arg::with_name("significance-level")
                .long("significance-level")
                .takes_value(true)
                .help(&format!("Changes the default significance level for this run. [default: {}]", self.config.significance_level)))
            .arg(Arg::with_name("test")
                .hidden(true)
                .long("test")
                .help("Run the benchmarks once, to verify that they execute successfully, but do not measure or report the results.")
                .conflicts_with_all(&["list", "profile-time"]))
            .arg(Arg::with_name("bench")
                .hidden(true)
                .long("bench"))
            .arg(Arg::with_name("plotting-backend")
                 .long("plotting-backend")
                 .takes_value(true)
                 .possible_values(&["gnuplot", "plotters"])
                 .help("Set the plotting backend. By default, Criterion.rs will use the gnuplot backend if gnuplot is available, or the plotters backend if it isn't."))
            .arg(Arg::with_name("output-format")
                .long("output-format")
                .takes_value(true)
                .possible_values(&["criterion", "bencher"])
                .default_value("criterion")
                .help("Change the CLI output format. By default, Criterion.rs will use its own format. If output format is set to 'bencher', Criterion.rs will print output in a format that resembles the 'bencher' crate."))
            .arg(Arg::with_name("nocapture")
                .long("nocapture")
                .hidden(true)
                .help("Ignored, but added for compatibility with libtest."))
            .arg(Arg::with_name("version")
                .hidden(true)
                .short("V")
                .long("version"))
            .after_help("
This executable is a Criterion.rs benchmark.
See https://github.com/bheisler/criterion.rs for more details.

To enable debug output, define the environment variable CRITERION_DEBUG.
Criterion.rs will output more debug information and will save the gnuplot
scripts alongside the generated plots.

To test that the benchmarks work, run `cargo test --benches`
")
            .get_matches();

        let bench = matches.is_present("bench");
        let test = matches.is_present("test");
        let test_mode = match (bench, test) {
            (true, true) => true,   // cargo bench -- --test should run tests
            (true, false) => false, // cargo bench should run benchmarks
            (false, _) => true,     // cargo test --benches should run tests
        };

        self.mode = if test_mode {
            Mode::Test
        } else if matches.is_present("list") {
            Mode::List
        } else if matches.is_present("profile-time") {
            let num_seconds = value_t!(matches.value_of("profile-time"), u64).unwrap_or_else(|e| {
                println!("{}", e);
                std::process::exit(1)
            });

            if num_seconds < 1 {
                println!("Profile time must be at least one second.");
                std::process::exit(1);
            }

            Mode::Profile(Duration::from_secs(num_seconds))
        } else {
            Mode::Benchmark
        };

        // This is kind of a hack, but disable the connection to the runner if we're not benchmarking.
        if !self.mode.is_benchmark() {
            self.connection = None;
        }

        if let Some(filter) = matches.value_of("FILTER") {
            self = self.with_filter(filter);
        }

        match matches.value_of("plotting-backend") {
            // Use plotting_backend() here to re-use the panic behavior if Gnuplot is not available.
            Some("gnuplot") => self = self.plotting_backend(PlottingBackend::Gnuplot),
            Some("plotters") => self = self.plotting_backend(PlottingBackend::Plotters),
            Some(val) => panic!("Unexpected plotting backend '{}'", val),
            None => {}
        }

        if matches.is_present("noplot") {
            self = self.without_plots();
        } else {
            self = self.with_plots();
        }

        if let Some(dir) = matches.value_of("save-baseline") {
            self.baseline = Baseline::Save;
            self.baseline_directory = dir.to_owned()
        }
        if let Some(dir) = matches.value_of("baseline") {
            self.baseline = Baseline::Compare;
            self.baseline_directory = dir.to_owned();
        }

        if self.connection.is_some() {
            // disable all reports when connected to cargo-criterion; it will do the reporting.
            self.report = Box::new(Reports::new(vec![]));
        } else {
            let cli_report: Box<dyn Report> = match matches.value_of("output-format") {
                Some("bencher") => Box::new(BencherReport),
                _ => {
                    let verbose = matches.is_present("verbose");
                    let stdout_isatty = atty::is(atty::Stream::Stdout);
                    let mut enable_text_overwrite = stdout_isatty && !verbose && !debug_enabled();
                    let enable_text_coloring;
                    match matches.value_of("color") {
                        Some("always") => {
                            enable_text_coloring = true;
                        }
                        Some("never") => {
                            enable_text_coloring = false;
                            enable_text_overwrite = false;
                        }
                        _ => enable_text_coloring = stdout_isatty,
                    };
                    Box::new(CliReport::new(
                        enable_text_overwrite,
                        enable_text_coloring,
                        verbose,
                    ))
                }
            };

            let mut reports: Vec<Box<dyn Report>> = vec![];
            reports.push(cli_report);
            reports.push(Box::new(FileCsvReport));
            if self.plotting_enabled {
                reports.push(Box::new(Html::new(self.create_plotter())));
            }
            self.report = Box::new(Reports::new(reports));
        }

        if let Some(dir) = matches.value_of("load-baseline") {
            self.load_baseline = Some(dir.to_owned());
        }

        if matches.is_present("sample-size") {
            let num_size = value_t!(matches.value_of("sample-size"), usize).unwrap_or_else(|e| {
                println!("{}", e);
                std::process::exit(1)
            });

            assert!(num_size >= 10);
            self.config.sample_size = num_size;
        }
        if matches.is_present("warm-up-time") {
            let num_seconds = value_t!(matches.value_of("warm-up-time"), u64).unwrap_or_else(|e| {
                println!("{}", e);
                std::process::exit(1)
            });

            let dur = std::time::Duration::new(num_seconds, 0);
            assert!(dur.to_nanos() > 0);

            self.config.warm_up_time = dur;
        }
        if matches.is_present("measurement-time") {
            let num_seconds =
                value_t!(matches.value_of("measurement-time"), u64).unwrap_or_else(|e| {
                    println!("{}", e);
                    std::process::exit(1)
                });

            let dur = std::time::Duration::new(num_seconds, 0);
            assert!(dur.to_nanos() > 0);

            self.config.measurement_time = dur;
        }
        if matches.is_present("nresamples") {
            let num_resamples =
                value_t!(matches.value_of("nresamples"), usize).unwrap_or_else(|e| {
                    println!("{}", e);
                    std::process::exit(1)
                });

            assert!(num_resamples > 0);

            self.config.nresamples = num_resamples;
        }
        if matches.is_present("noise-threshold") {
            let num_noise_threshold = value_t!(matches.value_of("noise-threshold"), f64)
                .unwrap_or_else(|e| {
                    println!("{}", e);
                    std::process::exit(1)
                });

            assert!(num_noise_threshold > 0.0);

            self.config.noise_threshold = num_noise_threshold;
        }
        if matches.is_present("confidence-level") {
            let num_confidence_level = value_t!(matches.value_of("confidence-level"), f64)
                .unwrap_or_else(|e| {
                    println!("{}", e);
                    std::process::exit(1)
                });

            assert!(num_confidence_level > 0.0 && num_confidence_level < 1.0);

            self.config.confidence_level = num_confidence_level;
        }
        if matches.is_present("significance-level") {
            let num_significance_level = value_t!(matches.value_of("significance-level"), f64)
                .unwrap_or_else(|e| {
                    println!("{}", e);
                    std::process::exit(1)
                });

            assert!(num_significance_level > 0.0 && num_significance_level < 1.0);

            self.config.significance_level = num_significance_level;
        }

        self
    }

    fn filter_matches(&self, id: &str) -> bool {
        match self.filter {
            Some(ref regex) => regex.is_match(id),
            None => true,
        }
    }

    /// Return a benchmark group. All benchmarks performed using a benchmark group will be
    /// grouped together in the final report.
    ///
    /// # Examples:
    ///
    /// ```rust
    /// #[macro_use] extern crate criterion;
    /// use self::criterion::*;
    ///
    /// fn bench_simple(c: &mut Criterion) {
    ///     let mut group = c.benchmark_group("My Group");
    ///
    ///     // Now we can perform benchmarks with this group
    ///     group.bench_function("Bench 1", |b| b.iter(|| 1 ));
    ///     group.bench_function("Bench 2", |b| b.iter(|| 2 ));
    ///    
    ///     group.finish();
    /// }
    /// criterion_group!(benches, bench_simple);
    /// criterion_main!(benches);
    /// ```
    pub fn benchmark_group<S: Into<String>>(&mut self, group_name: S) -> BenchmarkGroup<'_, M> {
        let group_name = group_name.into();

        if let Some(conn) = &self.connection {
            conn.send(&OutgoingMessage::BeginningBenchmarkGroup { group: &group_name })
                .unwrap();
        }

        BenchmarkGroup::new(self, group_name)
    }
}
impl<M> Criterion<M>
where
    M: Measurement + 'static,
{
    /// Benchmarks a function. For comparing multiple functions, see `benchmark_group`.
    ///
    /// # Example
    ///
    /// ```rust
    /// #[macro_use] extern crate criterion;
    /// use self::criterion::*;
    ///
    /// fn bench(c: &mut Criterion) {
    ///     // Setup (construct data, allocate memory, etc)
    ///     c.bench_function(
    ///         "function_name",
    ///         |b| b.iter(|| {
    ///             // Code to benchmark goes here
    ///         }),
    ///     );
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    pub fn bench_function<F>(&mut self, id: &str, f: F) -> &mut Criterion<M>
    where
        F: FnMut(&mut Bencher<'_, M>),
    {
        self.benchmark_group(id)
            .bench_function(BenchmarkId::no_function(), f);
        self
    }

    /// Benchmarks a function with an input. For comparing multiple functions or multiple inputs,
    /// see `benchmark_group`.
    ///
    /// # Example
    ///
    /// ```rust
    /// #[macro_use] extern crate criterion;
    /// use self::criterion::*;
    ///
    /// fn bench(c: &mut Criterion) {
    ///     // Setup (construct data, allocate memory, etc)
    ///     let input = 5u64;
    ///     c.bench_with_input(
    ///         BenchmarkId::new("function_name", input), &input,
    ///         |b, i| b.iter(|| {
    ///             // Code to benchmark using input `i` goes here
    ///         }),
    ///     );
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    pub fn bench_with_input<F, I>(&mut self, id: BenchmarkId, input: &I, f: F) -> &mut Criterion<M>
    where
        F: FnMut(&mut Bencher<'_, M>, &I),
    {
        // Guaranteed safe because external callers can't create benchmark IDs without a function
        // name or parameter
        let group_name = id.function_name.unwrap();
        let parameter = id.parameter.unwrap();
        self.benchmark_group(group_name).bench_with_input(
            BenchmarkId::no_function_with_input(parameter),
            input,
            f,
        );
        self
    }

    /// Benchmarks a function under various inputs
    ///
    /// This is a convenience method to execute several related benchmarks. Each benchmark will
    /// receive the id: `${id}/${input}`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate criterion;
    /// # use self::criterion::*;
    ///
    /// fn bench(c: &mut Criterion) {
    ///     c.bench_function_over_inputs("from_elem",
    ///         |b: &mut Bencher, size: &usize| {
    ///             b.iter(|| vec![0u8; *size]);
    ///         },
    ///         vec![1024, 2048, 4096]
    ///     );
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    #[doc(hidden)] // Soft-deprecated, use benchmark groups instead
    pub fn bench_function_over_inputs<I, F>(
        &mut self,
        id: &str,
        f: F,
        inputs: I,
    ) -> &mut Criterion<M>
    where
        I: IntoIterator,
        I::Item: fmt::Debug + 'static,
        F: FnMut(&mut Bencher<'_, M>, &I::Item) + 'static,
    {
        self.bench(id, ParameterizedBenchmark::new(id, f, inputs))
    }

    /// Benchmarks multiple functions
    ///
    /// All functions get the same input and are compared with the other implementations.
    /// Works similar to `bench_function`, but with multiple functions.
    ///
    /// # Example
    ///
    /// ``` rust
    /// # #[macro_use] extern crate criterion;
    /// # use self::criterion::*;
    /// # fn seq_fib(i: &u32) {}
    /// # fn par_fib(i: &u32) {}
    ///
    /// fn bench_seq_fib(b: &mut Bencher, i: &u32) {
    ///     b.iter(|| {
    ///         seq_fib(i);
    ///     });
    /// }
    ///
    /// fn bench_par_fib(b: &mut Bencher, i: &u32) {
    ///     b.iter(|| {
    ///         par_fib(i);
    ///     });
    /// }
    ///
    /// fn bench(c: &mut Criterion) {
    ///     let sequential_fib = Fun::new("Sequential", bench_seq_fib);
    ///     let parallel_fib = Fun::new("Parallel", bench_par_fib);
    ///     let funs = vec![sequential_fib, parallel_fib];
    ///
    ///     c.bench_functions("Fibonacci", funs, 14);
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    #[doc(hidden)] // Soft-deprecated, use benchmark groups instead
    pub fn bench_functions<I>(
        &mut self,
        id: &str,
        funs: Vec<Fun<I, M>>,
        input: I,
    ) -> &mut Criterion<M>
    where
        I: fmt::Debug + 'static,
    {
        let benchmark = ParameterizedBenchmark::with_functions(
            funs.into_iter().map(|fun| fun.f).collect(),
            vec![input],
        );

        self.bench(id, benchmark)
    }

    /// Executes the given benchmark. Use this variant to execute benchmarks
    /// with complex configuration. This can be used to compare multiple
    /// functions, execute benchmarks with custom configuration settings and
    /// more. See the Benchmark and ParameterizedBenchmark structs for more
    /// information.
    ///
    /// ```rust
    /// # #[macro_use] extern crate criterion;
    /// # use criterion::*;
    /// # fn routine_1() {}
    /// # fn routine_2() {}
    ///
    /// fn bench(c: &mut Criterion) {
    ///     // Setup (construct data, allocate memory, etc)
    ///     c.bench(
    ///         "routines",
    ///         Benchmark::new("routine_1", |b| b.iter(|| routine_1()))
    ///             .with_function("routine_2", |b| b.iter(|| routine_2()))
    ///             .sample_size(50)
    ///     );
    /// }
    ///
    /// criterion_group!(benches, bench);
    /// criterion_main!(benches);
    /// ```
    #[doc(hidden)] // Soft-deprecated, use benchmark groups instead
    pub fn bench<B: BenchmarkDefinition<M>>(
        &mut self,
        group_id: &str,
        benchmark: B,
    ) -> &mut Criterion<M> {
        benchmark.run(group_id, self);
        self
    }
}

trait DurationExt {
    fn to_nanos(&self) -> u64;
}

const NANOS_PER_SEC: u64 = 1_000_000_000;

impl DurationExt for Duration {
    fn to_nanos(&self) -> u64 {
        self.as_secs() * NANOS_PER_SEC + u64::from(self.subsec_nanos())
    }
}

/// Enum representing different ways of measuring the throughput of benchmarked code.
/// If the throughput setting is configured for a benchmark then the estimated throughput will
/// be reported as well as the time per iteration.
// TODO: Remove serialize/deserialize from the public API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Throughput {
    /// Measure throughput in terms of bytes/second. The value should be the number of bytes
    /// processed by one iteration of the benchmarked code. Typically, this would be the length of
    /// an input string or `&[u8]`.
    Bytes(u64),

    /// Measure throughput in terms of elements/second. The value should be the number of elements
    /// processed by one iteration of the benchmarked code. Typically, this would be the size of a
    /// collection, but could also be the number of lines of input text or the number of values to
    /// parse.
    Elements(u64),
}

/// Axis scaling type
#[derive(Debug, Clone, Copy)]
pub enum AxisScale {
    /// Axes scale linearly
    Linear,

    /// Axes scale logarithmically
    Logarithmic,
}

/// Contains the configuration options for the plots generated by a particular benchmark
/// or benchmark group.
///
/// ```rust
/// use self::criterion::{Bencher, Criterion, Benchmark, PlotConfiguration, AxisScale};
///
/// let plot_config = PlotConfiguration::default()
///     .summary_scale(AxisScale::Logarithmic);
///
/// // Using Criterion::default() for simplicity; normally you'd use the macros.
/// let mut criterion = Criterion::default();
/// let mut benchmark_group = criterion.benchmark_group("Group name");
/// benchmark_group.plot_config(plot_config);
/// // Use benchmark group
/// ```
#[derive(Debug, Clone)]
pub struct PlotConfiguration {
    summary_scale: AxisScale,
}

impl Default for PlotConfiguration {
    fn default() -> PlotConfiguration {
        PlotConfiguration {
            summary_scale: AxisScale::Linear,
        }
    }
}

impl PlotConfiguration {
    /// Set the axis scale (linear or logarithmic) for the summary plots. Typically, you would
    /// set this to logarithmic if benchmarking over a range of inputs which scale exponentially.
    /// Defaults to linear.
    pub fn summary_scale(mut self, new_scale: AxisScale) -> PlotConfiguration {
        self.summary_scale = new_scale;
        self
    }
}

/// Custom-test-framework runner. Should not be called directly.
#[doc(hidden)]
pub fn runner(benches: &[&dyn Fn()]) {
    for bench in benches {
        bench();
    }
    Criterion::default().configure_from_args().final_summary();
}
