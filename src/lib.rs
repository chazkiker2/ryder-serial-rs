use std::collections::VecDeque;
use std::io::{self, Write};
use std::result::Result;
use std::thread;

use ascii::IntoAsciiString;
use serialport::{SerialPortInfo, SerialPortType};

const RESPONSE_OK: u8 = 1;
const RESPONSE_SEND_INPUT: u8 = 2;
const RESPONSE_REJECTED: u8 = 3;
const RESPONSE_OUTPUT: u8 = 4;
const RESPONSE_OUTPUT_END: u8 = 5;
const RESPONSE_ESC_SEQUENCE: u8 = 6;
const RESPONSE_WAIT_USER_CONFIRM: u8 = 7;
const RESPONSE_LOCKED: u8 = 8;

const COMMAND_WAKE: u8 = 1;
const COMMAND_INFO: u8 = 2;
const COMMAND_SETUP: u8 = 10;
const COMMAND_RESTORE_FROM_SEED: u8 = 11;
const COMMAND_RESTORE_FROM_MNEMONIC: u8 = 12;
const COMMAND_ERASE: u8 = 13;

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

struct Options {
    log_level: u8,
    reject_on_locked: bool,
    reconnect_time: u32,
    debug: bool,
}

impl RyderSerial {
    fn new(port_name: Option<&str>) -> Result<Self, String> {
        let port_name = match port_name {
            Some(p) => String::from(p),
            None => {
                let ryder_devices = enumerate_devices();

                if ryder_devices.is_empty() {
                    return Err(String::from("No ryder device found!"));
                }

                if ryder_devices.len() > 1 {
                    println!(
                        "Multiple ryder devices were found! You must specify which device to use."
                    );
                    ryder_devices
                        .into_iter()
                        .for_each(|ryder_device| println!("{:#?}", ryder_device));
                    return Err(String::from(
                        "Multiple ryder devices were found! You must specify which device to use.",
                    ));
                }
                ryder_devices[0].port_name.clone()
            }
        };

        let mut port = serialport::new(port_name.clone(), 115_200)
            .open()
            .expect("Failed to open serial port");

        println!("Successfully Opened Ryder Port at: {}", &port_name);

        Ok(RyderSerial {
            id: 0,
            port_name: String::from(""),
            options: Options {
                log_level: 0_u8,
                reject_on_locked: true,
                reconnect_time: 3000,
                debug: true,
            },
            serial: port,
            state: State::SENDING,
            train: VecDeque::new(),
            lock: VecDeque::new(),
            results: Vec::new(),
        })
    }

    fn send(&mut self, command_buffer: &[u8]) -> Result<Vec<u8>, String> {
        let mut clone = self.serial.try_clone().expect("Failed to clone");

        let command_buffer = command_buffer.to_owned();
        println!("Sending Data: {:?}", command_buffer);
        thread::spawn(move || {
            clone
                .write_all(&command_buffer)
                .expect("Failed to write to serial port");
        });

        let mut buffer: [u8; 1000] = [0; 1000];

        loop {
            match self.serial.read(&mut buffer) {
                Ok(bytes) => {
                    let data = &buffer[..bytes];
                    println!(
                        "Received {} bytes.\nData: {:?}\nData ASCII: {:#?}",
                        bytes,
                        data,
                        data.into_ascii_string()
                    );

                    self.serial_data(data);

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

    fn ryder_next(&self) {
        if State::IDLE == self.state && !self.train.is_empty() {
            println!("NEXT... ryderserial is moving to next task");
        } else {
            println!("IDLE... ryderserial is waiting for next task");
        }
    }

    fn serial_data(&mut self, data: &[u8]) {
        println!("Data from Ryder: {:#?}", &data[..].into_ascii_string());
        let mut offset = 0;
        match self.state {
            State::IDLE => {
                println!("Received data from Ryder without asking. Discarding.");
                return;
            }
            State::SENDING => {
                println!("SENDING... ryder-serial is trying to send data");

                match data[0] {
                    RESPONSE_LOCKED => {
                        println!(
                            "RESPONSE_LOCKED -- RYDER DEVICE IS NEVER SUPPOSED TO EMIT THIS EVENT"
                        );
                        if self.options.reject_on_locked {
                            println!("Rejecting on Lock");
                            // create a ErrorLocked and reject all remaining train entries
                            // set state to idle
                        }
                        // emit locked
                        return;
                    }
                    RESPONSE_OK | RESPONSE_SEND_INPUT | RESPONSE_REJECTED => {
                        println!("RESPONSE_OK | RESPONSE_SEND_INPUT | RESPONSE_REJECTED");
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
                            println!("ryderserial more in buffer");
                            return self.serial_data(&data[1..]);
                        }
                        self.state = State::IDLE;
                        self.ryder_next();
                        return;
                    }
                    RESPONSE_OUTPUT => {
                        println!("RESPONSE_OUTPUT... ryderserial is ready to read");
                        self.state = State::READING;
                        self.serial_data(&data[1..]);
                        return;
                    }
                    RESPONSE_WAIT_USER_CONFIRM => {
                        // TODO: emit await user confirm
                        println!("waiting for user to confirm on device");
                        if data.len() > 1 {
                            println!("ryderserial more in buffer");
                            self.serial_data(&data[1..]);
                        }
                        return;
                    }
                    _ => {
                        // error
                        println!("ERROR -- (while sending): ryderserial ran into an error");
                        self.state = State::IDLE;
                        if data.len() > 1 {
                            println!("ryderserial more in buffer");
                            self.serial_data(&data[1..]);
                            return;
                        }
                        self.ryder_next();
                        return;
                    }
                }
            }
            State::READING => {
                println!("READING... ryderserial is trying to read data");

                // - the ryderserial is trying to read data
                // - initialize watchdog timeout

                let new_entry = TrainEntry {
                    data: Vec::new(),
                    is_prev_escaped_byte: false,
                    output_buffer: Vec::new(),
                };
                self.train.push_front(new_entry);

                for i in offset..data.len() {
                    let b = data[i];

                    if let Some(entry) = self.train.front_mut() {
                        match b {
                            RESPONSE_ESC_SEQUENCE => continue,
                            RESPONSE_OUTPUT_END => {
                                println!(
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

pub fn run() {
    let mut ryder_serial = RyderSerial::new(None).expect("failed to make Ryder Serial");
    let data = ryder_serial
        .send(&[COMMAND_INFO])
        .expect("Failed to send data");

    println!("{:?}", data);
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
