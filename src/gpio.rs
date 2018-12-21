// Copyright (c) 2017-2018 Rene van der Meer
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Interface for the GPIO peripheral.
//!
//! To ensure fast performance, RPPAL interfaces with the GPIO peripheral by
//! directly accessing the registers through either `/dev/gpiomem` or `/dev/mem`.
//! GPIO interrupts are controlled using the `/dev/gpiochipN` (where N=0, 1 and 2)
//! character device.
//!
//! ## Pins
//!
//! Pins are addressed by their BCM numbers, rather than their
//! physical location.
//!
//! By default, pins are reset to their original state when they go out of scope.
//! Use [`InputPin::set_clear_on_drop(false)`], [`OutputPin::set_clear_on_drop(false)`]
//! or [`AltPin::set_clear_on_drop(false)`], respecively, to disable this behavior.
//! Note that `drop` methods aren't called when a program is abnormally terminated (for
//! instance when a SIGINT isn't caught).
//!
//! ## Single instance
//!
//! Only a single [`Gpio`] instance can exist at any time. Multiple instances could
//! cause race conditions or pin configuration issues when several threads write to
//! the same register simultaneously. While other applications can't be prevented from
//! writing to the GPIO registers at the same time, limiting [`Gpio`] to a single instance
//! will at least make the Rust interface thread-safe.
//!
//! Constructing another instance before the existing one goes out of scope will return
//! an [`Error::InstanceExists`]. You can share a [`Gpio`] instance with other
//! threads using channels, cloning an `Arc<Mutex<Gpio>>` or globally sharing
//! a `Mutex<Gpio>`.
//!
//! ## Permission denied
//!
//! In recent releases of Raspbian (December 2017 or later), users that are part of the
//! `gpio` group (like the default `pi` user) can access `/dev/gpiomem` and
//! `/dev/gpiochipN` without needing additional permissions. If you encounter any
//! Permission Denied errors when creating a new [`Gpio`] instance, either the current
//! user isn't a member of the `gpio` group, or your Raspbian distribution isn't
//! up-to-date and doesn't automatically configure permissions for the above-mentioned
//! files. Updating Raspbian to the latest release should fix any permission issues.
//! Alternatively, although not recommended, you can run your application with superuser
//! privileges by using `sudo`.
//!
//! [`Gpio`]: struct.Gpio.html
//! [`InputPin::set_clear_on_drop(false)`]: struct.InputPin.html#method.set_clear_on_drop
//! [`OutputPin::set_clear_on_drop(false)`]: struct.InputPin.html#method.set_clear_on_drop
//! [`AltPin::set_clear_on_drop(false)`]: struct.InputPin.html#method.set_clear_on_drop
//! [`Error::InstanceExists`]: enum.Error.html#variant.InstanceExists
//!
//! ## Examples
//!
//! Basic example:
//!
//! ```
//! use std::thread::sleep;
//! use std::time::Duration;
//!
//! use rppal::gpio::Gpio;
//!
//! # fn main() -> rppal::gpio::Result<()> {
//! let gpio = Gpio::new()?;
//! let mut pin = gpio.get(23).unwrap().into_output();
//!
//! pin.set_high();
//! sleep(Duration::from_secs(1));
//! pin.set_low();
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::io;
use std::os::unix::io::AsRawFd;
use std::result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lazy_static::lazy_static;
use quick_error::quick_error;

mod epoll;
mod interrupt;
mod ioctl;
mod mem;
mod pin;

pub use self::pin::{AltPin, InputPin, OutputPin, Pin};

// Limit Gpio to a single instance
static mut GPIO_INSTANCED: AtomicBool = AtomicBool::new(false);

// Continue to keep track of taken pins when Gpio goes out of scope
lazy_static! {
    static ref PINS_TAKEN: [AtomicBool; pin::MAX] = init_array!(AtomicBool::new(false); pin::MAX);
}

quick_error! {
/// Errors that can occur when accessing the GPIO peripheral.
    #[derive(Debug)]
    pub enum Error {
/// Unknown SoC.
///
/// It wasn't possible to automatically identify the Raspberry Pi's SoC.
        UnknownSoC { description("unknown SoC") }
/// Permission denied when opening `/dev/gpiomem` and/or `/dev/mem` for read/write access.
///
/// Make sure the user has read and write access to `/dev/gpiomem`.
/// Common causes are either incorrect file permissions on `/dev/gpiomem`, or
/// the user isn't a member of the `gpio` group. If `/dev/gpiomem` is missing, upgrade to a more
/// recent version of Raspbian.
///
/// `/dev/mem` is a fallback when `/dev/gpiomem` can't be accessed. Getting read and write
/// access to `/dev/mem` is typically accomplished by executing the program as a
/// privileged user through `sudo`. A better solution that doesn't require `sudo` would be
/// to upgrade to a version of Raspbian that implements `/dev/gpiomem`.
        PermissionDenied { description("/dev/gpiomem and/or /dev/mem insufficient permissions") }
/// An instance of [`Gpio`] already exists.
///
/// Multiple instances of [`Gpio`] can cause race conditions or pin configuration issues when
/// several threads write to the same register simultaneously. While other applications
/// can't be prevented from writing to the GPIO registers at the same time, limiting [`Gpio`]
/// to a single instance will at least make the Rust interface less error-prone.
///
/// You can share a [`Gpio`] instance with other threads using channels, or cloning an
/// `Arc<Mutex<Gpio>>`. Although discouraged, you could also share it globally
/// wrapped in a `Mutex` using the `lazy_static` crate.
///
/// [`Gpio`]: struct.Gpio.html
        InstanceExists { description("an instance of Gpio already exists") }
/// IO error.
        Io(err: io::Error) { description(err.description()) from() }
/// Interrupt polling thread panicked.
        ThreadPanic { description("interrupt polling thread panicked") }
    }
}

/// Result type returned from methods that can have `rppal::gpio::Error`s.
pub type Result<T> = result::Result<T, Error>;

/// Pin modes.
#[derive(Debug, PartialEq, Copy, Clone)]
#[repr(u8)]
pub enum Mode {
    Input = 0b000,
    Output = 0b001,
    Alt5 = 0b010, // PWM
    Alt4 = 0b011, // SPI
    Alt0 = 0b100, // PCM
    Alt1 = 0b101, // SMI
    Alt2 = 0b110, // ---
    Alt3 = 0b111, // BSC-SPI
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Mode::Input => write!(f, "In"),
            Mode::Output => write!(f, "Out"),
            Mode::Alt0 => write!(f, "Alt0"),
            Mode::Alt1 => write!(f, "Alt1"),
            Mode::Alt2 => write!(f, "Alt2"),
            Mode::Alt3 => write!(f, "Alt3"),
            Mode::Alt4 => write!(f, "Alt4"),
            Mode::Alt5 => write!(f, "Alt5"),
        }
    }
}

/// Pin logic levels.
#[derive(Debug, PartialEq, Copy, Clone)]
#[repr(u8)]
pub enum Level {
    Low = 0,
    High = 1,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Level::Low => write!(f, "Low"),
            Level::High => write!(f, "High"),
        }
    }
}

/// Built-in pull-up/pull-down resistor states.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum PullUpDown {
    Off = 0b00,
    PullDown = 0b01,
    PullUp = 0b10,
}

impl fmt::Display for PullUpDown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PullUpDown::Off => write!(f, "Off"),
            PullUpDown::PullDown => write!(f, "PullDown"),
            PullUpDown::PullUp => write!(f, "PullUp"),
        }
    }
}

/// Interrupt trigger conditions.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum Trigger {
    Disabled = 0,
    RisingEdge = 1,
    FallingEdge = 2,
    Both = 3,
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Trigger::Disabled => write!(f, "Disabled"),
            Trigger::RisingEdge => write!(f, "RisingEdge"),
            Trigger::FallingEdge => write!(f, "FallingEdge"),
            Trigger::Both => write!(f, "Both"),
        }
    }
}

/// Provides access to the Raspberry Pi's GPIO peripheral.
pub struct Gpio {
    pub(crate) gpio_mem: Arc<mem::GpioMem>,
    cdev: Arc<std::fs::File>,
    sync_interrupts: Arc<Mutex<interrupt::EventLoop>>,
}

impl Gpio {
    /// Constructs a new `Gpio`.
    ///
    /// Only a single instance of `Gpio` can exist at any time. Constructing
    /// another instance before the existing one goes out of scope will return
    /// an [`Error::InstanceExists`]. You can share a `Gpio` instance with other
    /// threads using channels, cloning an `Arc<Mutex<Gpio>>` or globally sharing
    /// a `Mutex<Gpio>`.
    ///
    /// [`Error::InstanceExists`]: enum.Error.html#variant.InstanceExists
    pub fn new() -> Result<Gpio> {
        // Check if a Gpio instance already exists before initializing everything
        unsafe {
            if GPIO_INSTANCED.load(Ordering::SeqCst) {
                return Err(Error::InstanceExists);
            }
        }

        let cdev = ioctl::find_driver()?;
        let cdev_fd = cdev.as_raw_fd();

        let cdev = Arc::new(cdev);
        let event_loop = Arc::new(Mutex::new(interrupt::EventLoop::new(cdev_fd, pin::MAX)?));
        let gpio_mem = Arc::new(mem::GpioMem::open()?);

        let gpio = Gpio {
            gpio_mem,
            cdev,
            sync_interrupts: event_loop,
        };

        unsafe {
            // Returns true if GPIO_INSTANCED was set to true on a different thread
            // while we were still initializing ourselves, otherwise atomically sets
            // it to true here
            if GPIO_INSTANCED.compare_and_swap(false, true, Ordering::SeqCst) {
                return Err(Error::InstanceExists);
            }
        }

        Ok(gpio)
    }

    /// Returns a [`Pin`] for the specified GPIO pin number.
    ///
    /// Retrieving a GPIO pin using `get` grants exclusive access to the GPIO
    /// pin through an owned [`Pin`]. If the selected pin number is already
    /// in use, `get` returns `None`. After a [`Pin`] goes out of scope, it can be retrieved
    /// again using `get`.
    ///
    /// [`Pin`]: struct.Pin.html
    pub fn get(&self, pin: u8) -> Option<pin::Pin> {
        if pin as usize >= pin::MAX {
            return None;
        }

        // Returns true if the pin is currently taken, otherwise atomically sets
        // it to true here
        if PINS_TAKEN[pin as usize].compare_and_swap(false, true, Ordering::SeqCst) {
            // Pin is currently taken
            None
        } else {
            // Return an owned Pin
            let pin_instance = pin::Pin::new(
                pin,
                self.sync_interrupts.clone(),
                self.gpio_mem.clone(),
                self.cdev.clone(),
            );

            Some(pin_instance)
        }
    }

    /// Blocks until an interrupt is triggered on any of the specified pins, or until a timeout occurs.
    ///
    /// This only works for pins that have been configured for synchronous interrupts using
    /// [`InputPin::set_interrupt`]. Asynchronous interrupt triggers are automatically polled on a separate thread.
    ///
    /// If `reset` is set to `false`, returns immediately if an interrupt trigger event was cached in a
    /// previous call to [`InputPin::poll_interrupt`] or `poll_interrupts`.
    /// If `reset` is set too `true`, clears any cached interrupt trigger events before polling.
    ///
    /// The `timeout` duration indicates how long the call to `poll_interrupts` will block while waiting
    /// for interrupt trigger events, after which an `Ok(None))` is returned.
    /// `timeout` can be set to `None` to wait indefinitely.
    ///
    /// When an interrupt event is triggered, `poll_interrupts` returns
    /// `Ok((&`[`InputPin`]`, `[`Level`]`))` containing the corresponding pin and logic level. If multiple events trigger
    /// at the same time, only the first one is returned. The remaining events are cached and will be returned
    /// the next time [`InputPin::poll_interrupt`] or `poll_interrupts` is called.
    ///
    /// [`InputPin::set_interrupt`]: struct.InputPin#method.set_interrupt
    /// [`InputPin::poll_interrupt`]: struct.InputPin#method.poll_interrupt
    /// [`InputPin`]: struct.InputPin
    /// [`Level`]: struct.Level
    pub fn poll_interrupts<'a>(
        &self,
        pins: &[&'a InputPin],
        reset: bool,
        timeout: Option<Duration>,
    ) -> Result<Option<(&'a InputPin, Level)>> {
        (*self.sync_interrupts.lock().unwrap()).poll(pins, reset, timeout)
    }
}

impl Drop for Gpio {
    fn drop(&mut self) {
        unsafe {
            GPIO_INSTANCED.store(false, Ordering::SeqCst);
        }
    }
}

impl fmt::Debug for Gpio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gpio")
            .field("gpio_mem", &*self.gpio_mem)
            .field("sync_interrupts", &format_args!("{{ .. }}"))
            .finish()
    }
}