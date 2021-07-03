# Ryder Serial Notes

## Interesting Errors:

### Send WAKE when Prototype is "powered off" but still connected

```bash
[2021-07-03T20:05:46Z DEBUG ryder_serial] Successfully Opened Ryder Port at: /dev/tty.usbserial-0215A40B
[2021-07-03T20:05:46Z DEBUG ryder_serial] Sending Data: [1]
[2021-07-03T20:05:56Z DEBUG ryder_serial] Received 64 bytes.
    Data: [101, 116, 115, 32, 74, 117, 108, 32, 50, 57, 32, 50, 48, 49, 57, 32, 49, 50, 58, 50, 49, 58, 52, 54, 13, 10, 13, 10, 114, 115, 116, 58, 48, 120, 49, 32, 40, 80, 79, 87, 69, 82, 79, 78, 95, 82, 69, 83, 69, 84, 41, 44, 98, 111, 111, 116, 58, 48, 120, 49, 51, 32, 40, 83]
    Data ASCII: Ok(
        "ets Jul 29 2019 12:21:46\r\n\r\nrst:0x1 (POWERON_RESET),boot:0x13 (S",
    )
[2021-07-03T20:05:56Z DEBUG ryder_serial] Data from Ryder: Ok(
        "ets Jul 29 2019 12:21:46\r\n\r\nrst:0x1 (POWERON_RESET),boot:0x13 (S",
    )
[2021-07-03T20:05:56Z DEBUG ryder_serial] SENDING... ryder-serial is trying to send data
[2021-07-03T20:05:56Z DEBUG ryder_serial] ERROR -- (while sending): ryderserial ran into an error
[2021-07-03T20:05:56Z DEBUG ryder_serial] ryderserial more in buffer
[2021-07-03T20:05:56Z DEBUG ryder_serial] Data from Ryder: Ok(
        "ts Jul 29 2019 12:21:46\r\n\r\nrst:0x1 (POWERON_RESET),boot:0x13 (S",
    )
[2021-07-03T20:05:56Z DEBUG ryder_serial] Received data from Ryder without asking. Discarding.
thread 'main' panicked at 'Failed to run: results are empty

Location:
    /Users/chazadmin/code/opensource/ryder-serial-rs/src/lib.rs:265:32', src/main.rs:16:31
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

## Serial Data

Takes in a buffer of `&[u8]`

- if self.state == State.IDLE
  - we received data without asking for it :: discard
  - return out

- if self.state == State.SENDING
  - clear watchdog timeout
  - if the train has no entries, return early
  - else:: peek front of train to get resolve and reject
  - declare an offset
  - match data[0]
    - Status.RESPONSE_LOCKED
      - Ryder Device is locked.
      - if options indicte to reject on locked:
        - make an error locked error and reject all remaining entries on train
        - set state to idle
        - emit locked
        - return
      - else::
        - simply emit locked
    - RESPONSE_OK | RESPONSE_SEND_INPUT | RESPONSE_REJECTED
      - officially remove from front of train
      - resolve current data
      - if there is more in the buffer,
        - recurse with the rest of the data
        - return
      - else ::
        - set our state to idle
        - call next
        - return
    - RESPONSE_OUTPUT
      - ryderserial is trying to read data sent from the Ryder
      - set state to READING
      - add one to offset
      - let the check for READING later in function handle the rest
    - RESPONSE_WAIT_USER_CONFIRM
      - the Ryder is waiting for the user to confirm
      - if there is more in the buffer :
        - recurse with rest of buffer
      - return
    - _ (ELSE)
      - make an error
      - if data[0] exists as a known response error:
        - use the known error
      - else ::
        - make an UNKNOWN_RESPONSE error
      - log
      - reject with the error
      - remove first entry from train
      - set state to IDLE
      - if more in buffer:
        - recurse with rest of buffer
      - this.next
      - return
  - AFTER ALL OF THE ABOVE
  - if state has become State.READING
    - the ryderserial is trying to read data
    - initialize watchdog timeout
    - `for i in (ofset..data.byteLength)` {
      - take the `byte` from `data[i]`
      - if `train.peek_front.is_prev_escaped_byte` ::
        - match `byte`:
          - `RESPONSE_ESC_SEQUENCE` :
            - note that this was an escape byte
            - continue forward
          - `RESPONSE_OUTPUT_END`
            - reading success, resolving output buffer
            - remove entry from front of train
            - resolve with STATUS_OK and data as entry.output_buffer
            - set state to IDLE
            - next()
            - return
      - set `train.peek_front.is_prev_escaped_byte` to false
      - add the current byte to the output buffer
    - } // end `for i in (offset..data.byteLength)`

## `serial_error`

takes an error
returns void

- `emit("error", error)`
- if train is NOT empty::
  - reject from last entry in train
- clear watchdog timeout
- set state to IDLE
- next()

## `serial_watchdog`

- if train is empty, return
- else ::
  - reject with ERROR_WATCHDOG from front of train
  - set state to idle
  - next

## `open`

- set `this.closing = false`
- if the serial port is already open ::
  - return out
- else if the serial port exists but it's closed
  - close the serial port connection
- default `baud_rate` to `115_200`
- default `this.options.lock` to `true`
- default `reconnect_time` to `1_000`
- open new serial port
- `this.serial.on("data", `
  - `data => this.serial_data.bind(this)(data))`
- `this.serial.on("error",`
  - `error => `
  - if serial exists and is not open, clear `reconnect` timeout
  - set `this.reconnect` to new interval
    - when `reconnect_time` expires, call `open`
  - `this.emit("failed", error)`
- ON CLOSE
  - log close
  - emit close
  - clear reconnect interval
  - if not closing, initialize reconnect interval
- ON OPEN
  - log ryderserial open
  - clear reconnect interval
  - emit open
  - next


## next

- if this.state = State::IDLE and train is not empty:
  - move on to next task
  - if serial is not defined or not open:
    - connection to port has shut down
    - reject ERROR_DISCONNECTED from front of train
    - this.clear()
    - return
  - else::
    - set state to SENDING
    - TRY
      - `this.serial.write(train.first.data)`
    - CATCH ERROR
      - log error
      - this.serial_error(error)
      - return
    - clear watchdog timeout
    - initialize watchdog timeout
- ELSE
  - ryder serial is IDLE
