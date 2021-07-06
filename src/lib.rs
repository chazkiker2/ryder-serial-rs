use std::collections::HashMap;
use std::convert::TryFrom;
use std::future::Future;
use std::io::{self, Write};
use std::mem;
use std::pin::Pin;
use std::result::Result;
use std::str;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};

use ascii::IntoAsciiString;
use eyre::{eyre, Error};
use fehler::{throw, throws};
use log;
use serialport::{SerialPortInfo, SerialPortType};

// ----------------------------------------- FUTURE ------------------------------------------
#[derive(Clone, Debug)]
pub struct Task {
    id: usize,
    reactor: Arc<Mutex<Box<Reactor>>>,
    command_buffer: Vec<u8>,
    output_buffer: Vec<u8>,
}

impl Task {
    fn new(reactor: Arc<Mutex<Box<Reactor>>>, command_buffer: Vec<u8>, id: usize) -> Self {
        Task {
            id,
            reactor,
            command_buffer,
            output_buffer: vec![],
        }
    }
}

impl Future for Task {
    type Output = Vec<u8>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut r = self.reactor.lock().unwrap();
        if r.is_ready(self.id) {
            *r.tasks.get_mut(&self.id).unwrap() = TaskState::Finished;
            // send command buffer
            let mut clone = r.serial.try_clone().unwrap();
            let command_buffer = self.command_buffer.clone();

            thread::spawn(move || {
                clone
                    .write_all(&command_buffer)
                    .expect("Failed to write to serial port");
            });

            let mut serial = r.serial.try_clone().unwrap();
            let mut buffer: [u8; 1000] = [0; 1000];
            let x = loop {
                match serial.read(&mut buffer) {
                    Ok(0) => break vec![],
                    Ok(bytes) => {
                        let data = &buffer[..bytes];

                        log::debug!(
                            "Received {} bytes.\nData: {:?}\nData ASCII: {:#?}",
                            bytes,
                            data,
                            data.into_ascii_string()
                        );

                        break data.to_owned().iter().cloned().collect();
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::TimedOut => (),
                    Err(e) => panic!("{:#?}", e),
                };
            };
            Poll::Ready(x)
        } else if r.tasks.contains_key(&self.id) {
            r.tasks
                .insert(self.id, TaskState::NotReady(cx.waker().clone()));
            Poll::Pending
        } else {
            r.register(self.id, cx.waker().clone());
            Poll::Pending
        }
    }
}

// ----------------------------------------- REACTOR -----------------------------------------

#[derive(Debug)]
enum TaskState {
    Ready,
    NotReady(Waker),
    Finished,
}

#[derive(Debug)]
enum Event {
    Close,
    SendTask(usize),
}

struct Reactor {
    dispatcher: Sender<Event>,
    handle: Option<JoinHandle<()>>,
    tasks: HashMap<usize, TaskState>,
    serial: Box<dyn serialport::SerialPort>,
}

impl std::fmt::Debug for Reactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reactor")
            .field("dispatcher", &self.dispatcher)
            .field("handle", &self.handle)
            .field("tasks", &self.tasks)
            .finish()
    }
}

impl Reactor {
    fn new(serial: Box<dyn serialport::SerialPort>) -> Arc<Mutex<Box<Self>>> {
        let (tx, rx) = channel::<Event>();
        let reactor = Arc::new(Mutex::new(Box::new(Reactor {
            dispatcher: tx,
            handle: None,
            tasks: HashMap::new(),
            serial: serial.try_clone().unwrap(),
        })));
        let reactor_clone = Arc::downgrade(&reactor);
        let handle = thread::spawn(move || {
            let mut handles = vec![];
            for event in rx {
                let reactor = reactor_clone.clone();
                match event {
                    Event::Close => break,
                    Event::SendTask(id) => {
                        let event_handle = thread::spawn(move || {
                            let reactor = reactor.upgrade().unwrap();
                            reactor.lock().map(|mut r| r.wake(id)).unwrap();
                        });
                        handles.push(event_handle);
                    }
                }
            }
            handles
                .into_iter()
                .for_each(|handle| handle.join().unwrap());
        });
        reactor.lock().map(|mut r| r.handle = Some(handle)).unwrap();
        reactor
    }

    fn wake(&mut self, id: usize) {
        let state = self.tasks.get_mut(&id).unwrap();
        match mem::replace(state, TaskState::Ready) {
            TaskState::NotReady(waker) => waker.wake(),
            TaskState::Finished => panic!("Called 'wake' twice on task: {}", id),
            _ => unreachable!(),
        }
    }

    fn register(&mut self, id: usize, waker: Waker) {
        if self.tasks.insert(id, TaskState::NotReady(waker)).is_some() {
            panic!("Tried to insert a task with id: '{}', twice!", id);
        }
        self.dispatcher.send(Event::SendTask(id)).unwrap();
    }

    fn is_ready(&self, id: usize) -> bool {
        self.tasks
            .get(&id)
            .map(|state| match state {
                TaskState::Ready => true,
                _ => false,
            })
            .unwrap_or(false)
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        self.dispatcher.send(Event::Close).unwrap();
        self.handle.take().map(|h| h.join().unwrap()).unwrap();
    }
}

// ------------------------- Ryder Serial ----------------------------

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

struct RyderSerial {
    task_id: usize,
    reactor: Arc<Mutex<Box<Reactor>>>,
}

impl RyderSerial {
    #[throws]
    fn new(port_name: Option<&str>) -> Self {
        let port_name = match port_name {
            Some(p) => String::from(p),
            None => {
                let ryder_devices = enumerate_devices();

                if ryder_devices.is_empty() {
                    throw!(eyre!("No ryder device found!"));
                }

                if ryder_devices.len() > 1 {
                    log::error!(
                        "Multiple ryder devices were found! You must specify which device to use."
                    );

                    ryder_devices
                        .into_iter()
                        .for_each(|ryder_device| log::debug!("{:#?}", ryder_device));

                    throw!(eyre!(
                        "Multiple ryder devices were found! You must specify which device to use."
                    ));
                }
                ryder_devices[0].port_name.clone()
            }
        };

        let port = serialport::new(port_name.clone(), 115_200).open()?;
        log::debug!("Successfully Opened Ryder Port at: {}", &port_name);

        let reactor = Reactor::new(port.try_clone().unwrap());

        RyderSerial {
            reactor,
            task_id: 0,
        }
    }

    async fn send(&mut self, command_buffer: &[u8]) -> Result<Vec<u8>, Error> {
        self.task_id += 1;

        let react = self.reactor.clone();
        let task_id = self.task_id.clone();
        let command_buffer = command_buffer.to_owned().iter().cloned().collect();

        let task_fut =
            tokio::task::spawn(async move { Task::new(react, command_buffer, task_id).await });

        match task_fut.await {
            Ok(s) => Ok(s),
            Err(e) => Err(eyre!("{}", e)),
        }
    }
}

#[repr(u8)]
#[derive(Debug)]
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

    // cancel command
    Cancel = 100,
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
        log::info!("DATA FROM INFO: {:?}", data);
        data.into_ascii_string()?.into()
    }

    /// Wakes the device, puts it in high-power mode and turns on the display. (The same as tapping the screen.)
    #[throws]
    async fn wake(&mut self) -> Vec<u8> {
        let data = self.send(&[Command::Wake as u8]).await?;
        log::info!("DATA FROM WAKE: {:?}", data);
        data
    }

    #[throws]
    async fn setup(&mut self) -> Vec<u8> {
        let data = self.send(&[Command::Setup as u8]).await?;
        log::info!("DATA FROM SETUP: {:?}", data);
        let res = self.send(&[Command::Cancel as u8]).await?;
        log::info!("DATA FROM CANCEL: {:?}", res);
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

#[throws]
pub async fn run() {
    let mut ryder_serial = RyderSerial::new(None).expect("failed to make Ryder Serial");

    ryder_serial.wake().await?;

    let version = ryder_serial.info().await?;
    println!("Received Data from async send #1 --- {:?}", version);

    let res = ryder_serial.info().await?;
    println!("data from async send #2 --- {:?}", res);
}
