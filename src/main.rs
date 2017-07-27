#![no_std]
#![feature(asm)]
#![feature(proc_macro)]
#![feature(abi_msp430_interrupt)]
#![feature(const_fn)]

extern crate bit_reverse;
use bit_reverse::BitwiseReverse;

extern crate msp430g2211;

extern crate msp430_rtfm as rtfm;
use rtfm::{app, Resource, Threshold};

extern crate msp430_atomic;
use msp430_atomic::AtomicBool;

mod keyfsm;
use keyfsm::{Cmd, ProcReply, Fsm};

mod keybuffer;
use keybuffer::{KeycodeBuffer, KeyIn, KeyOut};

mod driver;
use driver::KeyboardPins;

#[cfg(feature = "use-timer")]
macro_rules! us_to_ticks {
    ($u:expr) => {
        // Timer is 100000 Hz, thus granularity of 10us.
        ($u / 10) + 1
    }
}

#[cfg(not(feature = "use-timer"))]
macro_rules! us_to_ticks {
    ($u:expr) => {
        // Delay is approx clock speed, thus granularity of 0.625us.
        ($u * 16) / 10
    }
}


#[cfg(feature = "use-timer")]
static TIMEOUT : AtomicBool = AtomicBool::new(false);
static HOST_MODE : AtomicBool = AtomicBool::new(false);
static DEVICE_ACK : AtomicBool = AtomicBool::new(false);

#[cfg(not(feature = "use-timer"))]
app! {
    device: msp430g2211,

    idle: {
        resources: [KEYBOARD_PINS, PORT_1_2, IN_BUFFER, KEY_IN, KEY_OUT],
    },

    resources: {
        static IN_BUFFER : KeycodeBuffer = KeycodeBuffer::new();
        static KEYBOARD_PINS : KeyboardPins = KeyboardPins::new();
        static KEY_IN : KeyIn = KeyIn::new();
        static KEY_OUT : KeyOut = KeyOut::new();
    },

    tasks: {
        PORT1: {
            path: porta_handler,
            resources: [KEYBOARD_PINS, PORT_1_2, IN_BUFFER, KEY_IN, KEY_OUT],
        },
    },
}

#[cfg(feature = "use-timer")]
app! {
    device: msp430g2211,

    idle: {
        resources: [KEYBOARD_PINS, TIMER_A2, PORT_1_2, IN_BUFFER, KEY_IN, KEY_OUT],
    },

    resources: {
        static IN_BUFFER : KeycodeBuffer = KeycodeBuffer::new();
        static KEYBOARD_PINS : KeyboardPins = KeyboardPins::new();
        static KEY_IN : KeyIn = KeyIn::new();
        static KEY_OUT : KeyOut = KeyOut::new();
    },

    tasks: {
        PORT1: {
            path: porta_handler,
            resources: [KEYBOARD_PINS, PORT_1_2, IN_BUFFER, KEY_IN, KEY_OUT],
        },

        TIMERA0: {
            path: timer0_handler,
            resources: [TIMER_A2],
        }
    },
}


#[cfg(feature = "use-timer")]
fn timer0_handler(_t: &mut Threshold, mut r: TIMERA0::Resources) {
    let timer = r.TIMER_A2;
    TIMEOUT.store(true);

    // Writing 0x0000 stops Timer in MC1.
    timer.taccr0.write(|w| unsafe { w.bits(0x0000) });
    // CCIFG will be reset when entering interrupt; no need to clear it.
    // Nesting is disabled, and chances of receiving second CCIFG in the ISR
    // are nonexistant.
}


fn porta_handler(_t: &mut Threshold, mut r: PORT1::Resources) {
    if HOST_MODE.load() {
        if !r.KEY_OUT.is_empty() {
            if r.KEY_OUT.shift_out() {
                r.KEYBOARD_PINS.at_data.set(&r.PORT_1_2);
            } else{
                r.KEYBOARD_PINS.at_data.unset(&r.PORT_1_2);
            }

            // Immediately after sending out the Stop Bit, we should release the lines.
            if r.KEY_OUT.is_empty() {
                r.KEYBOARD_PINS.at_idle(r.PORT_1_2);
            }
        } else {
            if r.KEYBOARD_PINS.at_data.is_unset(r.PORT_1_2) {
                DEVICE_ACK.store(true);
                r.KEY_OUT.clear();
            }
        }

        r.KEYBOARD_PINS.clear_at_clk_int(r.PORT_1_2);
    } else {
        let full : bool;

        // Are the buffer functions safe in nested interrupts? Is it possible to use tokens/manual
        // sync for nested interrupts while not giving up safety?
        // Example: Counter for nest level when updating buffers. If it's ever more than one, panic.
        r.KEY_IN.shift_in(r.KEYBOARD_PINS.at_data.is_set(r.PORT_1_2));
        full = r.KEY_IN.is_full();

        if full {
            r.KEYBOARD_PINS.at_inhibit(r.PORT_1_2); // Ask keyboard to not send anything while processing keycode.

            r.IN_BUFFER.put(r.KEY_IN.take().unwrap());
            r.KEY_IN.clear();

            r.KEYBOARD_PINS.at_idle(r.PORT_1_2);
        }

        r.KEYBOARD_PINS.clear_at_clk_int(r.PORT_1_2);
    }
}


fn init(p: init::Peripherals, r: init::Resources) {
    p.WATCHDOG_TIMER.wdtctl.write(|w| unsafe {
        const PASSWORD: u16 = 0x5A00;
        w.bits(PASSWORD).wdthold().set_bit()
    });

    // Make port idle
    r.KEYBOARD_PINS.idle(p.PORT_1_2);

    p.SYSTEM_CLOCK.bcsctl1.write(|w| w.xt2off().set_bit()
        .rsel3().set_bit()); // XT2 off, Range Select 7.
    p.SYSTEM_CLOCK.bcsctl2.write(|w| w.divs().divs_2()); // Divide submain clock by 4.

    #[cfg(feature = "use-timer")]
    {
        p.TIMER_A2.taccr0.write(|w| unsafe { w.bits(0x0000) });
        p.TIMER_A2.tactl.write(|w| w.tassel().tassel_2()
            .id().id_2().mc().mc_1());
        p.TIMER_A2.tacctl0.write(|w| w.ccie().set_bit());
    }
}

fn idle(t: &mut Threshold, mut r: idle::Resources) -> ! {
    send_byte_to_at_keyboard(t, &mut r, 0xFF);

    let mut loop_cmd : Cmd;
    let mut loop_reply : ProcReply = ProcReply::init();
    let mut fsm_driver : Fsm = Fsm::start();

    'get_command: loop {
        // Run state machine/send reply. Receive new cmd.
        loop_cmd = fsm_driver.run(&loop_reply).unwrap();

        loop_reply = match loop_cmd {
            Cmd::ClearBuffer => {
                rtfm::atomic(t, |cs| {
                    r.IN_BUFFER.borrow_mut(cs).flush();
                });
                ProcReply::ClearedBuffer
            },
            Cmd::ToggleLed(m) => {
                toggle_leds(t, &mut r, m);
                ProcReply::LedToggled(m)
            }
            Cmd::SendXTKey(k) => {
                send_byte_to_pc(t, &mut r, k);
                ProcReply::SentKey(k)
            },
            Cmd::WaitForKey => {
                // The micro spends the majority of its life idle. It is possible for the host PC and
                // the keyboard to send data to the micro at the same time. To keep control flow simple,
                // the micro will only respond to host PC acknowledge requests if its idle.
                let mut xt_reset : bool = false;
                'idle: while rtfm::atomic(t, |cs| { r.IN_BUFFER.borrow(cs).is_empty() }) {
                    // If host computer wants to reset
                    if rtfm::atomic(t, |cs| {
                        r.KEYBOARD_PINS.borrow(cs)
                            .xt_sense.is_unset(r.PORT_1_2.borrow(cs))
                    }) {
                        send_byte_to_at_keyboard(t, &mut r, 0xFF);
                        send_byte_to_pc(t, &mut r, 0xAA);
                        xt_reset = true;
                        break;
                    }
                }

                if xt_reset {
                    ProcReply::KeyboardReset
                } else {
                    let mut bits_in = rtfm::atomic(t, |cs|{
                        r.IN_BUFFER.borrow_mut(cs).take().unwrap()
                    });

                    bits_in = bits_in & !(0x4000 + 0x0001); // Mask out start/stop bit.
                    bits_in = bits_in >> 2; // Remove stop bit and parity bit (FIXME: Check parity).
                    ProcReply::GrabbedKey((bits_in as u8).swap_bits())
                }
            },

        }
    }
}

pub fn send_xt_bit(t: &mut Threshold, r: &mut idle::Resources, bit : u8) -> () {
    rtfm::atomic(t, |cs| {
        let pins = r.KEYBOARD_PINS.borrow(cs);
        let port = r.PORT_1_2.borrow(cs);
        if bit == 1 {
            pins.xt_data.set(port);
        } else {
            pins.xt_data.unset(port);
        }

        pins.xt_clk.unset(port);
    });

    delay(t, r, us_to_ticks!(55));

    rtfm::atomic(t, |cs| {
        r.KEYBOARD_PINS.borrow(cs)
            .xt_clk.set(r.PORT_1_2.borrow(cs));
    });
}

pub fn send_byte_to_pc(t: &mut Threshold, r: &mut idle::Resources, mut byte : u8) -> () {
    // The host cannot send data; the only communication it can do with the micro is pull
    // the CLK (reset) and DATA (shift register full) low.
    // Wait for the host to release the lines.

    while rtfm::atomic(t, |cs| {
        let pins = r.KEYBOARD_PINS.borrow(cs);
        let port = r.PORT_1_2.borrow(cs);
        pins.xt_clk.is_unset(port) || pins.xt_data.is_unset(port)
    }) { }

    rtfm::atomic(t, |cs| {
        r.KEYBOARD_PINS.borrow(cs).xt_out(r.PORT_1_2.borrow(cs));
    });

    send_xt_bit(t, r, 0);
    send_xt_bit(t, r, 1);

    for _ in 0..8 {
        send_xt_bit(t, r, (byte & 0x01)); /* Send data... */
		byte = byte >> 1;
    }

    rtfm::atomic(t, |cs| {
        r.KEYBOARD_PINS.borrow(cs).xt_in(r.PORT_1_2.borrow(cs));
    });
}

fn send_byte_to_at_keyboard(t: &mut Threshold, r: &mut idle::Resources, byte : u8) -> () {
    rtfm::atomic(t, |cs| {
        let mut key_out = r.KEY_OUT.borrow_mut(cs);
        key_out.put(byte).unwrap();
        // Safe outside of critical section: As long as HOST_MODE is
        // not set, it's not possible for the interrupt
        // context to touch this variable.
        r.KEYBOARD_PINS.borrow(cs)
            .disable_at_clk_int(r.PORT_1_2.borrow(cs));
    });

    /* If/when timer int is enabled, this loop really needs to allow preemption during
    I/O read. Can it be done without overhead of CriticalSection? */
    while rtfm::atomic(t, |cs| {
        r.KEYBOARD_PINS.borrow(cs)
            .at_clk.is_unset(r.PORT_1_2.borrow(cs))
    }) { }


    rtfm::atomic(t, |cs| {
        r.KEYBOARD_PINS.borrow(cs)
            .at_inhibit(r.PORT_1_2.borrow(cs));
    });

    delay(t, r, us_to_ticks!(100));

    rtfm::atomic(t, |cs| {
        r.KEYBOARD_PINS.borrow(cs)
            .at_data.unset(r.PORT_1_2.borrow(cs));
    });

    delay(t, r, us_to_ticks!(33));

    rtfm::atomic(t, |cs| {
        let pins = r.KEYBOARD_PINS.borrow(cs);
        let port = r.PORT_1_2.borrow(cs);
        pins.at_clk.set(port);
        pins.at_clk.mk_in(port);
        pins.clear_at_clk_int(port);

        unsafe {
            pins.enable_at_clk_int(port);
        }
        HOST_MODE.store(true);
        DEVICE_ACK.store(false);
    });

    while !DEVICE_ACK.load() { }

    HOST_MODE.store(false);
}

fn toggle_leds(t: &mut Threshold, r: &mut idle::Resources, mask : u8) -> () {
    send_byte_to_at_keyboard(t, r, 0xED);
    delay(t, r, us_to_ticks!(3000));
    send_byte_to_at_keyboard(t, r, mask);
}

#[cfg(not(feature = "use-timer"))]
fn delay(t: &mut Threshold, r: &mut idle::Resources, n : u16) {
    let _ = t;
    let _ = r;
    unsafe {
        asm!(r#"
1:
    dec $0
    jne 1b
    "# :: "{r12}"(n) : "r12" : "volatile");
    }
}

#[cfg(feature = "use-timer")]
fn delay(t: &mut Threshold, r: &mut idle::Resources, time : u16) {
    start_timer(t, r, time);
    while !TIMEOUT.load() {

    }
}

#[cfg(feature = "use-timer")]
fn start_timer(t: &mut Threshold, r: &mut idle::Resources, time : u16) -> () {
    rtfm::atomic(t, |cs| {
        let timer = r.TIMER_A2.borrow(cs);
        TIMEOUT.store(false);
        timer.taccr0.write(|w| unsafe { w.bits(time) });
    })
}
