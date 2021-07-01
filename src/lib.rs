use std::collections::VecDeque;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

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

struct TrainEntry {
    data: Vec<u8>,
    is_prev_escaped_byte: bool,
    output_buffer: Vec<u8>,
}

impl TrainEntry {
    pub fn resolve() {}
    pub fn reject() {}
}

struct LockEntry;
impl LockEntry {
    pub fn resolve() {}
    pub fn reject() {}
}

struct RyderSerial {
    id: u32,
    port_name: String,
    options: Options,
    serial: Box<dyn serialport::SerialPort>,
    state: State,
    train: VecDeque<TrainEntry>,
    lock: VecDeque<LockEntry>,
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
    fn new(port_name: Option<&str>) -> Self {
        let ryder_port = port_name.unwrap_or("/dev/tty.usbserial-0215A40B");

        let mut port = serialport::new(ryder_port, 9600)
            .open()
            .expect("Failed to open port");

        let bytes_written = port.write(&[COMMAND_INFO]).unwrap();
        println!("{}", bytes_written);
        let mut serial_buf: Vec<u8> = vec![0; 32];

        let bytes_read = port.read(serial_buf.as_mut_slice()).expect("Found no data");
        println!("{}", bytes_read);
        println!("{:?}", serial_buf);

        RyderSerial {
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
        }
    }

    fn ryder_next(&self) {
        if State::IDLE == self.state && self.train.is_empty() {
            println!("-> NEXT... ryderserial is moving to next task");
            // if self.serial.
        } else {
            println!("-> IDLE... ryderserial is waiting for next task");
        }
    }

    fn serial_data(&mut self, data: &[u8]) {
        println!("Data from Ryder: {:#?}", &data[..].into_ascii_string());

        if State::IDLE == self.state {
            println!("Received data from Ryder without asking. Discarding.");
            return;
        }

        let mut offset = 0;

        if State::SENDING == self.state {
            match data[0] {
                RESPONSE_LOCKED => {
                    println!(
                        "RESPONSE_LOCKED -- RYDER DEVICE IS NEVER SUPPOSED TO EMIT THIS EVENT"
                    );
                    if self.options.reject_on_locked {
                        println!("Rejecting on Lock");
                        return;
                    }
                }
                RESPONSE_OK | RESPONSE_SEND_INPUT | RESPONSE_REJECTED => {
                    println!("---> (while sending): RESPONSE_OK or RESPONSE_SEND_INPUT or RESPONSE_REJECTED");
                    if data.len() > 1 {
                        println!("ryderserial more in buffer");
                        return self.serial_data(&data[1..]);
                    }
                    self.state = State::IDLE;
                    self.ryder_next();
                    return;
                }
                RESPONSE_OUTPUT => {
                    println!(
                        "---> (while sending): RESPONSE_OUTPUT... ryderserial is ready to read"
                    );
                    self.state = State::READING;
                    offset += 1;
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
                    println!("---> ERROR -- (while sending): ryderserial ran into an error");
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

        if State::READING == self.state {
            println!(
                "---> (during response_output): READING... ryderserial is trying to read data"
            );
            for i in offset..data.len() {
                let b = data[i];
                println!("new byte {}", b);
                return;
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
    let ryder_devices: Vec<SerialPortInfo> = enumerate_devices();
    if ryder_devices.len() > 1 {
        println!("Multiple ryder devices were found! You must specify which device to use.");
        ryder_devices
            .into_iter()
            .for_each(|ryder_device| println!("{:#?}", ryder_device));
        return;
    }

    let port_name = ryder_devices[0].port_name.clone();

    let mut port = serialport::new(port_name, 115_200)
        .open()
        .expect("Failed to open serial port");

    let mut clone = port.try_clone().expect("Failed to clone");

    thread::spawn(move || {
        clone
            .write_all(&[COMMAND_INFO])
            .expect("Failed to write to serial port");
        thread::sleep(Duration::from_millis(1000));
    });
    let mut buffer: [u8; 1000] = [0; 1000];

    loop {
        match port.read(&mut buffer) {
            Ok(bytes) => {
                let data = &buffer[..bytes];
                println!("Received {} bytes.\nData: {:?}", bytes, &buffer[..bytes]);
                println!("Data as ASCII: {:#?}", &data[..].into_ascii_string());
            }
            Err(ref e) if e.kind() == io::ErrorKind::TimedOut => (),
            Err(e) => eprintln!("{:?}", e),
        }
    }

    // match port {
    //     Ok(mut port) => {
    //         let mut serial_buf: Vec<u8> = vec![0; 1000];
    //         println!("Receiving data on {} at {} baud:", &port_name, &115_200);
    //         loop {
    //             port.write(&[COMMAND_INFO]).unwrap();
    //             match port.read(serial_buf.as_mut_slice()) {
    //                 Ok(t) => io::stdout().write_all(&serial_buf[..t]).unwrap(),
    //                 Err(ref e) if e.kind() == io::ErrorKind::TimedOut => (),
    //                 Err(e) => eprintln!("{:?}", e),
    //             }
    //         }
    //     }
    //     Err(e) => {
    //         eprintln!("Failed to open \"{}\". Error: {}", port_name, e);
    //         ::std::process::exit(1);
    //     }
    // }

    // let ryder_port = "/dev/tty.usbserial-0215A40B";

    // let mut port = serialport::new(ryder_port, 9600)
    //     .open()
    //     .expect("Failed to open port");

    // let mut ryder_serial = RyderSerial::new(None);
    // let res = ryder_serial.serial.write(&[COMMAND_INFO]).unwrap();
    // println!("{}", res);

    // ryder_serial.serial_data(&[COMMAND_INFO])

    // let bytes_written = port.write(&"Hello, from Rust".as_bytes()).unwrap();
    // println!("{}", bytes_written);
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
