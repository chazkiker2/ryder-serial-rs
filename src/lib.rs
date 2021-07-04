use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::future::Future;
use std::io::{self, Write};
use std::mem;
use std::pin::Pin;
use std::result::Result;
use std::str;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread::{self, JoinHandle};

use ascii::IntoAsciiString;
use eyre::{eyre, Error};
use fehler::{throw, throws};
use log::{debug, error, info};
use serialport::{SerialPortInfo, SerialPortType};

// ============================= EXECUTOR ====================================
#[derive(Default)]
struct Parker(Mutex<bool>, Condvar);

impl Parker {
    fn park(&self) {
        // We acquire a lock to the Mutex which protects our flag indicating if we
        // should resume execution or not.
        let mut resumable = self.0.lock().unwrap();

        // We put this in a loop since there is a chance we'll get woken, but
        // our flag hasn't changed. If that happens, we simply go back to sleep.
        while !*resumable {
            // We sleep until someone notifies us
            resumable = self.1.wait(resumable).unwrap();
        }

        // We immediately set the condition to false, so that next time we call `park` we'll
        // go right to sleep.
        *resumable = false;
    }

    fn unpark(&self) {
        // We simply acquire a lock to our flag and sets the condition to `runnable` when we
        // get it.
        *self.0.lock().unwrap() = true;

        // We notify our `Condvar` so it wakes up and resumes.
        self.1.notify_one();
    }
}

fn block_on<F: Future>(mut future: F) -> F::Output {
    let parker = Arc::new(Parker::default());
    let ryder_waker = Arc::new(RyderWaker {
        parker: parker.clone(),
    });
    let waker = ryder_waker_into_waker(Arc::into_raw(ryder_waker));
    let mut cx = Context::from_waker(&waker);

    // SAFETY: we shadow `future` so it can't be accessed again.
    let mut future = unsafe { Pin::new_unchecked(&mut future) };
    loop {
        match Future::poll(future.as_mut(), &mut cx) {
            Poll::Ready(val) => break val,
            Poll::Pending => parker.park(),
        };
    }
}

// ----------------------------------------- FUTURE ------------------------------------------
#[derive(Clone)]
struct RyderWaker {
    parker: Arc<Parker>,
}
#[derive(Clone, Debug)]
pub struct Task {
    id: usize,
    reactor: Arc<Mutex<Box<Reactor>>>,
    command_buffer: Vec<u8>,
    output_buffer: Vec<u8>,
}

fn ryder_waker_wake(s: &RyderWaker) {
    let waker_arc = unsafe { Arc::from_raw(s) };
    waker_arc.parker.unpark();
}

fn ryder_waker_clone(s: &RyderWaker) -> RawWaker {
    let arc = unsafe { Arc::from_raw(s) };
    std::mem::forget(arc.clone()); // increase ref count
    RawWaker::new(Arc::into_raw(arc) as *const (), &VTABLE)
}

const VTABLE: RawWakerVTable = unsafe {
    RawWakerVTable::new(
        |s| ryder_waker_clone(&*(s as *const RyderWaker)), // clone
        |s| ryder_waker_wake(&*(s as *const RyderWaker)),  // wake
        |s| (*(s as *const RyderWaker)).parker.unpark(),   // wake by ref (don't decrease refcount)
        |s| drop(Arc::from_raw(s as *const RyderWaker)),   // decrease refcount
    )
};

fn ryder_waker_into_waker(s: *const RyderWaker) -> Waker {
    let raw_waker = RawWaker::new(s as *const (), &VTABLE);
    unsafe { Waker::from_raw(raw_waker) }
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

                        debug!(
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
            r.register(
                self.id,
                // self.id,
                // self.command_buffer,
                // &mut self.output_buffer,
                cx.waker().clone(),
            );
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
            // .field("serial", )
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
                            // thread::sleep(Duration::from_secs(duration));
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
        self.dispatcher
            // .send(Event::SendTask(id, command_buffer, output_buffer.clone()))
            .send(Event::SendTask(id))
            .unwrap();
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

#[throws]
pub async fn run() {
    let mut ryder_serial = RyderSerial::new(None, None).expect("failed to make Ryder Serial");

    ryder_serial.wake().await?;

    let version = ryder_serial.info().await?;
    println!("Received Data from async send #1 --- {:?}", version);

    let res = ryder_serial.info().await?;
    println!("{:?}", res);
}

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
    command_buffer: Vec<u8>,
    is_prev_escaped_byte: bool,
    output_buffer: Vec<u8>,
}

struct LockEntry;

struct RyderSerial {
    id: u32,
    task_id: usize,
    port_name: String,
    options: Options,
    serial: Box<dyn serialport::SerialPort>,
    state: State,
    reactor: Arc<Mutex<Box<Reactor>>>,
    // train_fut: VecDeque<RyderFuture>,
    train: VecDeque<TrainEntry>,
    lock: VecDeque<LockEntry>,
}

#[derive(PartialEq)]
enum State {
    IDLE,
    SENDING,
    READING,
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
        debug!("Successfully Opened Ryder Port at: {}", &port_name);

        let reactor = Reactor::new(port.try_clone().unwrap());

        RyderSerial {
            id: 0,
            port_name,
            options: options_in.into(),
            serial: port,
            state: State::IDLE,
            reactor,
            train: VecDeque::new(),
            lock: VecDeque::new(),

            task_id: 0,
        }
    }

    async fn send(&mut self, command_buffer: &[u8]) -> Result<Vec<u8>, Error> {
        self.task_id += 1;
        let new_fut = async {
            let val = Task::new(
                self.reactor.clone(),
                command_buffer.to_owned().iter().cloned().collect(),
                self.task_id,
            )
            .await;
            val
        };

        let main_fut = async { new_fut.await };

        Ok(block_on(main_fut))
    }

    fn ryder_next(&mut self) -> Result<(), Error> {
        if State::IDLE == self.state && !self.train.is_empty() {
            debug!("NEXT... ryder-serial is moving to next task");

            // if device is closed, reject with ERROR_DISCONNECTED and self.clear
            self.state = State::SENDING;
            let entry = self.train.front().unwrap();
            let command_buffer = entry.command_buffer.clone();

            let mut clone = self.serial.try_clone()?;
            thread::spawn(move || {
                clone
                    .write_all(&command_buffer)
                    .expect("Failed to write to serial port");
            });
        } else {
            debug!("IDLE... ryder-serial is waiting for next task");
        }
        Ok(())
    }

    #[throws]
    fn serial_data(&mut self, data: &[u8]) -> () {
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
                                return self.serial_data(&data[1..])?;
                            }
                            self.state = State::IDLE;
                            self.ryder_next()?;
                            return;
                        }
                        Response::Output => {
                            debug!("RESPONSE_OUTPUT... ryderserial is ready to read");
                            self.state = State::READING;
                            self.serial_data(&data[1..])?;
                            return;
                        }
                        Response::WaitUserConfirm => {
                            // TODO: emit await user confirm
                            debug!("waiting for user to confirm on device");
                            if data.len() > 1 {
                                println!("ryderserial more in buffer");
                                self.serial_data(&data[1..])?;
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
                            self.serial_data(&data[1..])?;
                            return;
                        }
                        self.ryder_next()?;
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

                    // if let Some(entry) = self.train.front_mut() {

                    if let Some(entry) = self.train.front_mut() {
                        // let mut inner = entry.inner.borrow_mut();
                        // let inner = Rc::clone(&entry.inner);

                        // if let None = inner.output_buffer {
                        //     inner.output_buffer = Some(Vec::new());
                        // }
                        match Response::try_from(b) {
                            Ok(Response::EscSequence) => continue,
                            Ok(Response::OutputEnd) => {
                                debug!(
                                    "READING SUCCESS resolving output buffer:\n{:#?}",
                                    &entry.output_buffer[..].into_ascii_string()
                                );
                                // resolve output buffer
                                // self.train_fut.pop_front().expect("Train was empty");
                                // self.train_fut.pop_front().expect("Train was empty");

                                // self.results.push(ResultEntry::Resolved {
                                //     buffer: entry.output_buffer,
                                // });
                                self.state = State::IDLE;
                                // self.ryder_next();
                                return;
                            }
                            _ => (),
                        };

                        entry.is_prev_escaped_byte = false;
                        entry.output_buffer.push(b);
                    }
                }
            }
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
    // // cancel command
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
        let res = self.send(&[Command::Cancel as u8]).await?;
        info!("DATA FROM CANCEL: {:?}", res);
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

// ------------------------------- RYDER SERIAL CONFIG -------------------------------
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

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
