use clap::{crate_authors, crate_version, value_t, values_t, App, Arg};
use failure::Fail;
use satnogs_network_client::Client;
use std::process;
use std::thread;
use systemstat::{Platform, System};

use crossbeam_channel::unbounded;
use notify::{RecursiveMode, Watcher, immediate_watcher};
use regex::Regex;
use byteorder::{LittleEndian, ReadBytesExt};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Seek, SeekFrom};

mod event;
mod job;
mod logger;
mod satnogs;
mod settings;
mod state;
mod station;
mod sysinfo;
mod ui;
mod vessel;
mod widgets;

use self::event::Event;
use self::settings::{Settings, StationConfig};
use self::station::Station;
use self::sysinfo::SysInfo;

type Result<T> = std::result::Result<T, failure::Error>;

#[derive(Debug, Fail)]
#[fail(display = "No station provided")]
struct NoStationError;

fn main() {
    if let Err(err) = run() {
        eprintln!("{}", format_error(&err));
        let backtrace = err.backtrace().to_string();
        if !backtrace.trim().is_empty() {
            eprintln!("{}", backtrace);
        }
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let settings = settings()?;
    // get the station info from the network
    let mut client = Client::new(&settings.api_endpoint)?;

    let mut state = state::State::new();

    for station in &settings.stations {
        state.add_station(
            client
                .station_info(station.satnogs_id)
                .map(|si| Station::new(si))?,
        );

        if state.active_station == 0 { 
            state.active_station = station.satnogs_id;
        }
    }

    state.update_ground_tracks(settings.ui.ground_track_num);

    let local_stations: Vec<_> = settings
        .stations
        .iter()
        .filter(|sc| sc.local)
        .map(|sc| sc.satnogs_id)
        .collect();
    let tui = ui::Ui::new(settings, client, state)?;
    log::set_boxed_logger(Box::new(logger::Logger::new(tui.sender())))?;

    if !local_stations.is_empty() {
        let tx = tui.sender();
        thread::spawn(move || {
            while let Ok(sys_info) = get_sysinfo() {
                match tx.send(Event::SystemInfo(local_stations.clone(), sys_info)) {
                    Ok(_) => thread::sleep(std::time::Duration::new(4, 0)),
                    Err(e) => {
                        log::error!("Failed to send system info: {}", e);
                        break;
                    }
                }
            }
        });

        // watch for waterfall if enabled
        let tx = tui.sender();
        thread::spawn(move || {
            let (watcher_tx, watcher_rx) = unbounded();
            if let Ok(mut watcher) = immediate_watcher(watcher_tx) {
                if watcher.watch("/tmp/.satnogs/data/", RecursiveMode::NonRecursive).is_ok() {
                    let mut fft_size = 0u64;
                    let mut observation = 0u64;
                    let mut file = None;
                    let re = Regex::new(r".*/.*receiving_waterfall_(\d+)_.*\.dat.*").unwrap();

                    let is_data_available = |reader: &mut BufReader<File>, fft_size: u64| {
                        let size = reader.get_ref().metadata().unwrap().len();
                        let position = reader.seek(SeekFrom::Current(0)).unwrap();

                        (size - position >= fft_size * 4 + 4)
                    };
 
                    loop {
                        match watcher_rx.recv() {
                            Ok(event) => {
                                if let Ok(op) = event.op {
                                    if op.contains(notify::Op::CREATE) {
                                        if let Some(path) = &event.path {
                                            log::info!("File created: {}", path.to_str().unwrap());

                                            if let Some(obs_id) = re.captures(path.to_str().unwrap()) {
                                                observation = obs_id[1].parse().unwrap();
                                                let reader = OpenOptions::new().read(true).open(path).ok();

                                                if let Some(waterfall) = reader {
                                                    while waterfall.metadata().unwrap().len() < 4 {}

                                                    let mut waterfall = BufReader::new(waterfall);
                                                    fft_size = waterfall.read_f32::<LittleEndian>().unwrap() as u64;

                                                    while waterfall.get_ref().metadata().unwrap().len() < 4 + 4 * fft_size {}

                                                    let mut frequencies = vec![];
                                                    frequencies.reserve(fft_size as usize);
                                                    for _ in 0..fft_size {
                                                        frequencies.push(waterfall.read_f32::<LittleEndian>().unwrap());
                                                    }

                                                    if let Err(err) = tx.send(Event::WaterfallCreated(observation, frequencies)) {
                                                        log::error!("Failed to send waterfall creation event: {}", err);
                                                    }
                                                    file = Some(waterfall);
                                                }
                                            };
                                        };
                                    }

                                    if op.contains(notify::Op::WRITE) {
                                        if let Some(mut waterfall) = file {
                                            if let Some(path) = &event.path {
                                                if let Some(obs_id) = re.captures(path.to_str().unwrap()) {
                                                    let obs_id: u64 = obs_id[1].parse().unwrap();
                                                    if obs_id != observation {
                                                        log::warn!("Waterfall doesn't match last observarion {} != {}", obs_id, observation);
                                                    } else {
                                                        log::info!("New waterfall data for observation {}", observation);

                                                        while is_data_available(&mut waterfall, fft_size) {
                                                            let seconds = waterfall.read_f32::<LittleEndian>().unwrap();
                                                            let mut data = vec![];
                                                            data.reserve(fft_size as usize);
                                                            for _ in 0..fft_size {
                                                                data.push(waterfall.read_f32::<LittleEndian>().unwrap());
                                                            }

                                                            if let Err(err) = tx.send(Event::WaterfallData(seconds, data)) {
                                                                log::error!("Failed to send waterfall data event: {}", err);
                                                            }
                                                        }

                                                    }
                                                };
                                            };
                                            file = Some(waterfall);
                                        };
                                    }

                                    if op.contains(notify::Op::CLOSE_WRITE) {
                                        if let Some(path) = &event.path {
                                            if let Some(obs_id) = re.captures(path.to_str().unwrap()) {
                                                let obs_id = obs_id[1].parse().unwrap();
                                                log::info!("Closed waterfall for observation {}", obs_id);
                                                if let Err(err) = tx.send(Event::WaterfallClosed(obs_id)) {
                                                    log::error!("Failed to send waterfall closing event: {}", err);
                                                }
                                            };
                                        };
                                    }
                                }
                            },
                            Err(err) => {
                                log::error!("Watcher error: {}. Stopping watcher.", err);
                                break;
                            }
                        }
                    }
                } else {
                    log::error!("Failed to watch waterfall directory.");
                }

            } else {
                log::error!("Failed to create waterfall watcher.");
            }
        });
    }

    tui.run()
}

fn get_sysinfo() -> Result<SysInfo> {
    let sys = System::new();
    let cpu_load = sys.cpu_load();
    thread::sleep(std::time::Duration::new(1, 0));

    Ok(SysInfo {
        cpu_load: cpu_load.and_then(|load| load.done()).ok(),
        cpu_temp: sys.cpu_temp().ok(),
        mem: sys.memory().ok(),
        uptime: sys.uptime().ok(),
    })
}

fn format_error(err: &failure::Error) -> String {
    let mut out = "Error occurred: ".to_string();
    out.push_str(&err.to_string());
    let mut prev = err.as_fail();
    while let Some(next) = prev.cause() {
        out.push_str("\n -> ");
        out.push_str(&next.to_string());
        prev = next;
    }
    out
}

fn settings() -> Result<Settings> {
    let app = App::new("satnogs-monitor")
        .version(crate_version!())
        .author(crate_authors!("\n"))
        .about("Monitors the current and future jobs of SatNOGS ground stations.")
        .max_term_width(100)
        .arg(
            Arg::with_name("api_url")
                .short("a")
                .long("api")
                .help("Sets the SatNOGS network api endpoint url")
                .value_name("URL")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .help("Sets custom config file")
                .value_name("FILE")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("local_station")
                .short("l")
                .long("local")
                .help(
                    "Adds a station running on the same machine as this monitor \
                     with this SatNOGS network id to to the list of monitored stations",
                )
                .value_name("ID")
                .takes_value(true)
                .multiple(true),
        )
        .arg(
            Arg::with_name("orbits")
                .short("o")
                .long("orbits")
                .help("Sets the number of orbits plotted on the map")
                .value_name("NUM")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("station")
                .short("s")
                .long("station")
                .help(
                    "Adds a station with this SatNOGS network id to the list of \
                     monitored stations",
                )
                .value_name("ID")
                .takes_value(true)
                .multiple(true),
        )
        .arg(
            Arg::with_name("verbosity")
                .short("v")
                .multiple(true)
                .help("Sets the level of log verbosity"),
        );

    let matches = app.get_matches();

    let mut settings = matches
        .value_of("config")
        .map_or(Settings::new(), |config| Settings::from_file(config))?;

    let log_level = std::cmp::max(
        matches.occurrences_of("verbosity"),
        settings.log_level.unwrap_or(0),
    );
    let log_filter = match log_level {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        2 => log::LevelFilter::Debug,
        _3_or_more => log::LevelFilter::Trace,
    };

    log::set_max_level(log_filter);

    if let Ok(api_endpoint) = value_t!(matches.value_of("api_url"), String) {
        settings.api_endpoint = api_endpoint;
    }

    if let Ok(ids) = values_t!(matches.values_of("local_station"), u64) {
        for id in ids {
            // if the station was already configured in the config file we just overwrite the local flag
            if let Some(sc) = settings.stations.iter_mut().find(|sc| sc.satnogs_id == id) {
                (*sc).local = true;
            } else {
                let mut sc = StationConfig::new(id);
                sc.local = true;;
                settings.stations.push(sc);
            }
        }
    }

    if let Ok(ids) = values_t!(matches.values_of("station"), u64) {
        for id in ids {
            if settings
                .stations
                .iter()
                .find(|&sc| sc.satnogs_id == id)
                .is_none()
            {
                settings.stations.push(StationConfig::new(id));
            }
        }
    }

    if settings.stations.is_empty() {
        return Err(NoStationError.into());
    }

    // only one entry per station
    settings.stations.sort_unstable_by_key(|sc| sc.satnogs_id);
    settings.stations.dedup_by_key(|sc| sc.satnogs_id);

    if let Ok(orbits) = value_t!(matches.value_of("orbits"), u8) {
        settings.ui.ground_track_num = std::cmp::max(1, orbits);
    }

    Ok(settings)
}
