use std::collections::VecDeque;
use std::convert::TryFrom;
use std::io::{self, Write};
use std::result::Result;
use std::thread;

use ascii::IntoAsciiString;
use eyre::{eyre, Error};
use fehler::{throw, throws};
use futures::future;
use log::{debug, error, info};
use serialport::{SerialPortInfo, SerialPortType};

#[repr(u8)]
enum Response {
    Ok = 1,
    SendInput,
    Rejected,
    Output,
    OutputEnd,
    EscSequence,
    WaitUserConfirm,
    Locked,
}

impl TryFrom<u8> for Response {
    type Error = ();

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            x if x == Response::Ok as u8 => Ok(Response::Ok),
            x if x == Response::SendInput as u8 => Ok(Response::SendInput),
            x if x == Response::Rejected as u8 => Ok(Response::Rejected),
            x if x == Response::Output as u8 => Ok(Response::Output),
            x if x == Response::OutputEnd as u8 => Ok(Response::OutputEnd),
            x if x == Response::EscSequence as u8 => Ok(Response::EscSequence),
            x if x == Response::WaitUserConfirm as u8 => Ok(Response::WaitUserConfirm),
            x if x == Response::Locked as u8 => Ok(Response::Locked),
            _ => Err(()),
        }
    }
}

#[derive(Debug)]
struct TrainEntry {
    data: Vec<u8>,
    is_prev_escaped_byte: bool,
    output_buffer: Vec<u8>,
}

#[derive(Debug)]
enum ResultEntry {
    Resolved { buffer: Vec<u8> },
    Rejected { entry: TrainEntry, error: String },
}

struct LockEntry;

struct RyderSerial {
    id: u32,
    port_name: String,
    options: Options,
    serial: Box<dyn serialport::SerialPort>,
    state: State,
    train: VecDeque<TrainEntry>,
    lock: VecDeque<LockEntry>,
    results: Vec<ResultEntry>,
}

#[derive(PartialEq)]
enum State {
    IDLE,
    SENDING,
    READING,
}

enum LogLevel {
    // Error,
    Warn,
    // Info,
    // Debug,
    // Trace,
}

impl Default for LogLevel {
    fn default() -> Self {
        LogLevel::Warn
    }
}

struct OptionsIn {
    log_level: Option<LogLevel>,
    reject_on_locked: Option<bool>,
    reconnect_time: Option<u32>,
    debug: Option<bool>,
}

struct Options {
    log_level: LogLevel,
    reject_on_locked: bool,
    reconnect_time: u32,
    debug: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            log_level: LogLevel::Warn,
            reject_on_locked: false,
            reconnect_time: 115_200,
            debug: false,
        }
    }
}

impl From<Option<OptionsIn>> for Options {
    fn from(other: Option<OptionsIn>) -> Options {
        match other {
            Some(OptionsIn {
                log_level,
                reject_on_locked,
                reconnect_time,
                debug,
            }) => Options {
                log_level: log_level.unwrap_or_default(),
                reject_on_locked: reject_on_locked.unwrap_or(false),
                reconnect_time: reconnect_time.unwrap_or(115_200),
                debug: debug.unwrap_or(false),
            },
            None => Options::default(),
        }
    }
}

impl RyderSerial {
    #[throws]
    fn new(port_name: Option<&str>, options_in: Option<OptionsIn>) -> Self {
        let port_name = match port_name {
            Some(p) => String::from(p),
            None => {
                let ryder_devices = enumerate_devices();

                if ryder_devices.is_empty() {
                    throw!(eyre!("No ryder device found!"));
                }

                if ryder_devices.len() > 1 {
                    error!(
                        "Multiple ryder devices were found! You must specify which device to use."
                    );

                    ryder_devices
                        .into_iter()
                        .for_each(|ryder_device| debug!("{:#?}", ryder_device));

                    throw!(eyre!(
                        "Multiple ryder devices were found! You must specify which device to use."
                    ));
                }
                ryder_devices[0].port_name.clone()
            }
        };

        let port = serialport::new(port_name.clone(), 115_200).open()?;
        // .expect("Failed to open serial port");

        debug!("Successfully Opened Ryder Port at: {}", &port_name);

        RyderSerial {
            id: 0,
            port_name,
            options: options_in.into(),
            serial: port,
            state: State::IDLE,
            train: VecDeque::new(),
            lock: VecDeque::new(),
            results: Vec::new(),
        }
    }

    fn send_no_async(&mut self, command_buffer: &[u8]) -> Result<Vec<u8>, String> {
        let mut clone = self.serial.try_clone().expect("Failed to clone");

        let command_buffer = command_buffer.to_owned();
        debug!("Sending Data: {:?}", command_buffer);

        thread::spawn(move || {
            clone
                .write_all(&command_buffer)
                .expect("Failed to write to serial port");
        });

        let mut buffer: [u8; 1000] = [0; 1000];
        self.state = State::SENDING;

        loop {
            match self.serial.read(&mut buffer) {
                Ok(bytes) => {
                    let data = &buffer[..bytes];

                    debug!(
                        "Received {} bytes.\nData: {:?}\nData ASCII: {:#?}",
                        bytes,
                        data,
                        data.into_ascii_string()
                    );

                    self.serial_data(data);

                    if self.results.is_empty() {
                        return Err(String::from("results are empty"));
                    }

                    return match self.results.remove(0) {
                        ResultEntry::Resolved { buffer } => Ok(buffer),
                        ResultEntry::Rejected { entry: _, error } => Err(error),
                    };
                }
                Err(ref e) if e.kind() == io::ErrorKind::TimedOut => (),
                Err(e) => eprintln!("{:?}", e),
            }
        }
    }

    #[throws]
    async fn send(&mut self, command_buffer: &[u8]) -> Vec<u8> {
        let mut clone = self.serial.try_clone()?;

        let command_buffer = command_buffer.to_owned();

        let new_entry = TrainEntry {
            data: command_buffer.to_owned().iter().cloned().collect(),
            is_prev_escaped_byte: false,
            output_buffer: Vec::new(),
        };

        debug!("Sending Data: {:?}", command_buffer);

        thread::spawn(move || {
            clone
                .write_all(&command_buffer)
                .expect("Failed to write to serial port");
        });

        let mut buffer: [u8; 1000] = [0; 1000];
        self.state = State::SENDING;

        self.train.push_front(new_entry);

        loop {
            match self.serial.read(&mut buffer) {
                Ok(bytes) => {
                    let data = &buffer[..bytes];

                    debug!(
                        "Received {} bytes.\nData: {:?}\nData ASCII: {:#?}",
                        bytes,
                        data,
                        data.into_ascii_string()
                    );

                    self.serial_data(data);

                    if self.results.is_empty() {
                        throw!(eyre!("results are empty"));
                    }

                    match self.results.remove(0) {
                        ResultEntry::Resolved { buffer } => {
                            return future::ok::<Vec<u8>, eyre::Error>(buffer).await?
                        }
                        ResultEntry::Rejected { entry: _, error } => throw!(eyre!(error)),
                    };
                }
                Err(ref e) if e.kind() == io::ErrorKind::TimedOut => (),
                Err(e) => throw!(eyre!(e)),
            }
        }

        unreachable!();
    }

    fn ryder_next(&self) {
        if State::IDLE == self.state && !self.train.is_empty() {
            debug!("NEXT... ryder-serial is moving to next task");
        } else {
            debug!("IDLE... ryder-serial is waiting for next task");
        }
    }

    fn serial_data(&mut self, data: &[u8]) {
        debug!("Data from Ryder: {:#?}", &data[..].into_ascii_string());

        match self.state {
            State::IDLE => {
                debug!("Received data from Ryder without asking. Discarding.");
                return;
            }

            // the ryder serial is trying to send data to the ryder
            State::SENDING => {
                debug!("SENDING... ryder-serial is trying to send data");

                match Response::try_from(data[0]) {
                    // known response -- handle accordingly
                    Ok(response) => match response {
                        Response::Locked => {
                            debug!(
                                "RESPONSE_LOCKED -- RYDER DEVICE IS NEVER SUPPOSED TO EMIT THIS EVENT"
                            );
                            if self.options.reject_on_locked {
                                debug!("Rejecting on Lock");
                                // create a ErrorLocked and reject all remaining train entries
                                // set state to idle
                            }
                            // emit locked
                            return;
                        }
                        Response::Ok | Response::SendInput | Response::Rejected => {
                            debug!("RESPONSE_OK | RESPONSE_SEND_INPUT | RESPONSE_REJECTED");
                            let entry = self.train.pop_front().expect("Train was empty");
                            debug!("Resolving at entry: {:#?}", entry);
                            self.results.push(ResultEntry::Resolved {
                                buffer: vec![data[0]],
                            });
                            // - officially remove from front of train
                            // - resolve current data
                            // - if there is more in the buffer,
                            //      - recurse with the rest of the data
                            //      - return
                            // - else ::
                            //      - set our state to idle
                            //      - call next
                            //      - return
                            if data.len() > 1 {
                                debug!("ryderserial more in buffer");
                                return self.serial_data(&data[1..]);
                            }
                            self.state = State::IDLE;
                            self.ryder_next();
                            return;
                        }
                        Response::Output => {
                            debug!("RESPONSE_OUTPUT... ryderserial is ready to read");
                            self.state = State::READING;
                            self.serial_data(&data[1..]);
                            return;
                        }
                        Response::WaitUserConfirm => {
                            // TODO: emit await user confirm
                            debug!("waiting for user to confirm on device");
                            if data.len() > 1 {
                                println!("ryderserial more in buffer");
                                self.serial_data(&data[1..]);
                            }
                            return;
                        }
                        // these members are not relevant when `self.state == State::SENDING`
                        Response::OutputEnd | Response::EscSequence => (),
                    },
                    // unknown response -- raise error accordingly
                    Err(_) => {
                        debug!("ERROR -- (while sending): ryderserial ran into an error");
                        // TODO: emit/raise error to the client
                        self.state = State::IDLE;
                        if data.len() > 1 {
                            debug!("ryderserial more in buffer");
                            self.serial_data(&data[1..]);
                            return;
                        }
                        self.ryder_next();
                        return;
                    }
                }
            }

            // the ryder serial is trying to read data from the ryder
            State::READING => {
                debug!("READING... ryderserial is trying to read data");

                // TODO: initialize watchdog timeout

                for i in 0..data.len() {
                    let b = data[i];

                    if let Some(entry) = self.train.front_mut() {
                        match Response::try_from(b) {
                            Ok(Response::EscSequence) => continue,
                            Ok(Response::OutputEnd) => {
                                debug!(
                                    "READING SUCCESS resolving output buffer:\n{:#?}",
                                    &entry.output_buffer[..].into_ascii_string()
                                );
                                // resolve output buffer
                                let entry = self.train.pop_front().expect("Train was empty");

                                self.results.push(ResultEntry::Resolved {
                                    buffer: entry.output_buffer,
                                });
                                self.state = State::IDLE;
                                self.ryder_next();
                                return;
                            }
                            _ => (),
                        }
                        entry.is_prev_escaped_byte = false;
                        entry.output_buffer.push(b);
                    }
                }
            }
        }
    }
}

#[repr(u8)]
enum Command {
    // // lifecycle commands
    Wake = 1,
    Info = 2,
    Setup = 10,
    // RestoreFromSeed = 11,
    // RestoreFromMnemonic = 12,
    // Erase = 13,
    // // export commands
    // ExportOwnerKey = 18,
    // ExportOwnerKeyPrivateKey = 19,
    // ExportAppKey = 20,
    // ExportAppKeyPrivateKey = 21,
    // ExportOwnerAppKeyPrivateKey = 23,
    // ExportPublicIdentities = 30,
    // ExportPublicIdentity = 31,
    // // encrypt / decrypt commands
    // StartEncrypt = 40,
    // StartDecrypt = 41,
    // // cancel command
    // Cancel = 100,
}

/// Convenience Methods
impl RyderSerial {
    /// Returns some information about the device in the following format:
    ///
    /// `ryder[VERSION_MAJOR][VERSION_MINOR][VERSION_PATCH][MODE][INITIALIZED]`
    ///
    /// - Raw Bytes: `[114, 121, 100, 101, 114, 0, 0, 1, 0, 1]`
    /// - As ASCII: `"ryder\u{0}\u{0}\u{1}\u{0}\u{1}"`
    /// - Version: `0.0.1`,
    /// - Mode: `0`,
    /// - Initialized: `1` (yes),
    #[throws]
    async fn info(&mut self) -> String {
        let data = self.send(&[Command::Info as u8]).await?;
        info!("DATA FROM INFO: {:?}", data);
        data.into_ascii_string()?.into()
    }

    /// Wakes the device, puts it in high-power mode and turns on the display. (The same as tapping the screen.)
    #[throws]
    async fn wake(&mut self) -> Vec<u8> {
        let data = self.send(&[Command::Wake as u8]).await?;
        info!("DATA FROM WAKE: {:?}", data);
        data
    }

    #[throws]
    async fn setup(&mut self) -> Vec<u8> {
        let data = self.send(&[Command::Setup as u8]).await?;
        info!("DATA FROM SETUP: {:?}", data);
        data
    }
}

/// Retrieve all Ryder devices from serialport connection
pub fn enumerate_devices() -> Vec<SerialPortInfo> {
    let devices = serialport::available_ports().expect("No ports found!");
    devices
        .into_iter()
        .filter(|device| match &device.port_type {
            SerialPortType::UsbPort(usb_port_info) => {
                usb_port_info.vid == 4292 && usb_port_info.pid == 60000
            }
            _ => false,
        })
        .collect()
}

pub fn run_no_async() {
    let mut ryder_serial = RyderSerial::new(None, None).expect("failed to make Ryder Serial");
    let data = ryder_serial
        .send_no_async(&[Command::Info as u8])
        .expect("Failed to send data");

    println!("Received Data from non-async send #1 --- {:?}", data);

    let data = ryder_serial
        .send_no_async(&[Command::Info as u8])
        .expect("Failed to send data");

    println!("Received Data from non-async send #2 --- {:?}", data);
}

#[throws]
pub async fn run() {
    let mut ryder_serial = RyderSerial::new(None, None).expect("failed to make Ryder Serial");

    ryder_serial.wake().await?;

    let version = ryder_serial.info().await?;
    println!("Received Data from async send #1 --- {:?}", version);

    let setup_res = ryder_serial.setup().await?;

    // let data = ryder_serial.send(&[Command::Info as u8]).await?;
    // println!("Received Data from async send #2 --- {:?}", data);
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
