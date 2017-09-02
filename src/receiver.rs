#![allow(deprecated)]

use clocksource::Clocksource;
use common::{self, ControlMessage, Interest, Percentile};
use config::Config;
use data::{Allans, Counters, Heatmaps, Histograms, Meters, Sample};
use mio::{Events, Poll, PollOpt, Ready, Token, channel};
use mpmc::Queue;
use sender::Sender;
use std::collections::HashSet;
use std::fmt::Display;
use std::hash::Hash;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tiny_http::{Request, Response, Server};

// define token numbers for data and control queues
const TOKEN_DATA: usize = 1;
const TOKEN_CONTROL: usize = 2;

/// a `Receiver` processes incoming `Sample`s and generates stats
pub struct Receiver<T> {
    window_time: u64,
    window_duration: u64,
    end_time: u64,
    run_duration: u64,
    config: Config<T>,
    empty_queue: Arc<Queue<Vec<Sample<T>>>>,
    data_rx: channel::Receiver<Vec<Sample<T>>>,
    data_tx: channel::SyncSender<Vec<Sample<T>>>,
    control_rx: channel::Receiver<ControlMessage<T>>,
    control_tx: channel::SyncSender<ControlMessage<T>>,
    allans: Allans<T>,
    counters: Counters<T>,
    histograms: Histograms<T>,
    meters: Meters<T>,
    interests: HashSet<Interest<T>>,
    taus: Vec<usize>,
    percentiles: Vec<Percentile>,
    heatmaps: Heatmaps<T>,
    server: Option<Server>,
    clocksource: Clocksource,
    poll: Poll,
}

impl<T: Hash + Eq + Send + Clone + Display> Default for Receiver<T> {
    /// create a default `Receiver`
    fn default() -> Self {
        Config::new().build()
    }
}

impl<T: Hash + Eq + Send + Display + Clone> Receiver<T> {
    /// create a new `Receiver` using the defaults
    pub fn new() -> Receiver<T> {
        Default::default()
    }

    /// create a `Receiver` from a tic::Config
    pub fn configured(config: Config<T>) -> Receiver<T> {
        let (data_tx, data_rx) = channel::sync_channel::<Vec<Sample<T>>>(config.capacity);
        let (control_tx, control_rx) = channel::sync_channel::<ControlMessage<T>>(config.capacity);
        let empty_queue = Arc::new(Queue::with_capacity(config.capacity));
        for _ in 0..config.capacity {
            let _ = empty_queue.push(Vec::with_capacity(config.batch_size));
        }

        let slices = config.duration * config.windows;

        let listen = config.http_listen.clone();
        let server = start_listener(&listen);

        let clocksource = Clocksource::default();

        // calculate counter values for start, window, and end times
        let start_time = clocksource.counter();
        let window_duration = (config.duration as f64 * clocksource.frequency()) as u64;
        let window_time = start_time + window_duration;
        let run_duration = config.windows as u64 * window_duration;
        let end_time = start_time + run_duration;

        let poll = Poll::new().unwrap();
        poll.register(
            &data_rx,
            Token(TOKEN_DATA),
            Ready::readable(),
            PollOpt::level(),
        ).unwrap();
        poll.register(
            &control_rx,
            Token(TOKEN_CONTROL),
            Ready::readable(),
            PollOpt::level(),
        ).unwrap();

        Receiver {
            window_duration: window_duration,
            window_time: window_time,
            run_duration: run_duration,
            end_time: end_time,
            config: config,
            empty_queue: empty_queue,
            data_tx: data_tx,
            data_rx: data_rx,
            control_tx: control_tx,
            control_rx: control_rx,
            allans: Allans::new(),
            counters: Counters::new(),
            histograms: Histograms::new(),
            meters: Meters::new(),
            interests: HashSet::new(),
            taus: common::default_taus(),
            percentiles: common::default_percentiles(),
            heatmaps: Heatmaps::new(slices, start_time),
            server: server,
            clocksource: clocksource,
            poll: poll,
        }
    }

    /// Create a new Config which can be used to build() a Receiver
    pub fn configure() -> Config<T> {
        Config::default()
    }

    /// returns a clone of the `Sender`
    pub fn get_sender(&self) -> Sender<T> {
        Sender::new(
            self.empty_queue.clone(),
            self.data_tx.clone(),
            self.control_tx.clone(),
            self.config.batch_size,
        )
    }

    /// returns a clone of the `Clocksource`
    pub fn get_clocksource(&self) -> Clocksource {
        self.clocksource.clone()
    }

    /// register a stat for export
    pub fn add_interest(&mut self, interest: Interest<T>) {
        match interest.clone() {
            Interest::AllanDeviation(key) => {
                self.allans.init(key);
            }
            Interest::Count(key) => {
                self.counters.init(key);
            }
            Interest::Percentile(key) => {
                self.histograms.init(key);
            }
            Interest::Trace(key, _) |
            Interest::Waterfall(key, _) => {
                self.heatmaps.init(key);
            }
        }
        self.interests.insert(interest);
    }

    /// de-register a stat for export
    pub fn remove_interest(&mut self, interest: &Interest<T>) {
        match interest.clone() {
            Interest::AllanDeviation(key) => {
                self.allans.remove(key);
            }
            Interest::Count(key) => {
                self.counters.remove(key);
            }
            Interest::Percentile(key) => {
                self.histograms.remove(key);
            }
            Interest::Trace(key, _) |
            Interest::Waterfall(key, _) => {
                self.heatmaps.remove(key);
            }
        }
        self.interests.remove(interest);
    }

    /// clear the heatmaps
    pub fn clear_heatmaps(&mut self) {
        self.heatmaps.clear();
    }

    /// run the receive loop for one window
    pub fn run_once(&mut self) {
        trace!("run once");

        let window_time = self.window_time;
        let mut http_time = self.clocksource.counter() +
            (0.1 * self.clocksource.frequency()) as u64;

        loop {
            if self.clocksource.counter() > http_time {
                self.try_handle_http(&self.server);
                http_time += (0.1 * self.clocksource.frequency()) as u64;
            }

            if self.check_elapsed(window_time) {
                return;
            }

            let mut events = Events::with_capacity(1024);
            self.poll.poll(&mut events, self.config.poll_delay).unwrap();
            for event in events.iter() {
                trace!("got: {} events", events.len());
                match event.token().0 {
                    TOKEN_DATA => {
                        if let Ok(mut results) = self.data_rx.try_recv() {
                            for result in &results {
                                let t0 = self.clocksource.convert(result.start());
                                let t1 = self.clocksource.convert(result.stop());
                                let dt = t1 - t0;
                                self.allans.record(result.metric(), dt);
                                self.counters.increment_by(result.metric(), result.count());
                                self.histograms.increment(result.metric(), dt as u64);
                                self.heatmaps.increment(
                                    result.metric(),
                                    t0 as u64,
                                    dt as u64,
                                );
                            }
                            results.clear();
                            let _ = self.empty_queue.push(results);
                            trace!("finished processing");
                        }
                    }
                    TOKEN_CONTROL => {
                        if let Ok(msg) = self.control_rx.try_recv() {
                            match msg {
                                ControlMessage::AddInterest(interest) => {
                                    self.add_interest(interest);
                                }
                                ControlMessage::RemoveInterest(interest) => {
                                    self.remove_interest(&interest);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            trace!("run complete");
        }
    }

    fn check_elapsed(&mut self, t1: u64) -> bool {
        let tsc = self.clocksource.counter();
        if tsc >= t1 {
            for interest in &self.interests {
                match *interest {
                    Interest::Count(ref key) => {
                        self.meters.set_count(
                            key.clone(),
                            self.counters.count(key.clone()),
                        );
                    }
                    Interest::Percentile(ref key) => {
                        for percentile in self.percentiles.clone() {
                            self.meters.set_percentile(
                                key.clone(),
                                percentile.clone(),
                                self.histograms
                                    .percentile(key.clone(), percentile.1)
                                    .unwrap_or(0),
                            );
                        }
                    }
                    Interest::AllanDeviation(ref key) => {
                        for tau in self.taus.clone() {
                            if let Some(adev) = self.allans.adev(key.clone(), tau) {
                                self.meters.set_adev(key.clone(), tau, adev);
                            }
                        }
                    }
                    Interest::Trace(_, _) |
                    Interest::Waterfall(_, _) => {}
                }
            }

            self.histograms.clear();
            self.window_time += self.window_duration;
            return true;
        }
        false
    }

    /// run the receive loop for all windows, output waterfall and traces as requested
    pub fn run(&mut self) {
        let mut window = 0;
        debug!("collection ready");
        'outer: loop {
            'inner: loop {
                self.run_once();
                window += 1;
                if window >= self.config.windows {
                    break 'inner;
                }
            }

            self.save_files();

            if !self.config.service_mode {
                break 'outer;
            } else {
                self.heatmaps.clear();
                self.end_time += self.run_duration;
            }
        }
    }

    /// save all artifacts
    pub fn save_files(&mut self) {
        for interest in self.interests.clone() {
            match interest {
                Interest::Trace(l, f) => {
                    self.heatmaps.trace(l, f);
                }
                Interest::Waterfall(l, f) => {
                    self.heatmaps.waterfall(l, f);
                }
                _ => {}
            }
        }
    }

    pub fn clone_meters(&self) -> Meters<T> {
        self.meters.clone()
    }

    // try to handle a http request
    fn try_handle_http(&self, server: &Option<Server>) {
        if let Some(ref s) = *server {
            if let Ok(Some(request)) = s.try_recv() {
                trace!("handle http request");
                self.handle_http(request);
            }
        }
    }

    // actually handle the http request
    fn handle_http(&self, request: Request) {
        let mut output = "".to_owned();

        match request.url() {
            "/vars" | "/metrics" => {
                for (stat, value) in &self.meters.data {
                    output = output + &format!("{} {}\n", stat, value);
                }
                for (stat, value) in &self.meters.data_float {
                    output = output + &format!("{} {}\n", stat, value);
                }
            }
            _ => {
                output += "{";
                for (stat, value) in &self.meters.data {
                    output = output + &format!("\"{}\":{},", stat, value);
                }
                for (stat, value) in &self.meters.data_float {
                    output = output + &format!("\"{}\":{},", stat, value);
                }
                output.pop();
                output += "}";
            }
        }

        let response = Response::from_string(output);
        let _ = request.respond(response);
    }
}

// start the HTTP listener for tic
fn start_listener(listen: &Option<String>) -> Option<Server> {
    if let Some(ref l) = *listen {
        let http_socket = l.to_socket_addrs().unwrap().next().unwrap();

        debug!("starting HTTP listener");
        return Some(Server::http(http_socket).unwrap());
    }
    None
}

#[cfg(feature = "benchmark")]
#[cfg(test)]
mod benchmark {
    extern crate test;
    use super::*;

    #[bench]
    fn heavy_cycle(b: &mut test::Bencher) {
        let mut receiver = Receiver::<String>::new();
        receiver.add_interest(Interest::Count("test".to_owned()));
        receiver.add_interest(Interest::Percentile("test".to_owned()));
        receiver.add_interest(Interest::AllanDeviation("test".to_owned()));
        b.iter(|| {
            // full stats evaluation
            receiver.check_elapsed(0);
        });
    }

    #[bench]
    fn cheap_cycle(b: &mut test::Bencher) {
        let mut receiver = Receiver::<String>::new();
        receiver.add_interest(Interest::Count("test".to_owned()));
        receiver.add_interest(Interest::Percentile("test".to_owned()));
        receiver.add_interest(Interest::AllanDeviation("test".to_owned()));
        b.iter(|| {
            // no stats evaluation just get clock and compare
            receiver.check_elapsed(u64::max_value());
        });
    }
}
