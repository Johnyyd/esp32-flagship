//! ╔══════════════════════════════════════════════════════════════╗
//! ║         ESP32 RUST FLAGSHIP — DỰ ÁN TỔNG HỢP              ║
//! ║   Gộp tất cả 9 bài học ESP32 Rust vào một chương trình     ║
//! ╚══════════════════════════════════════════════════════════════╝
//!
//! Kiến trúc CLI Shell — gõ lệnh qua Serial Monitor để chọn tính năng:
//!
//!   help            — Hiển thị danh sách lệnh
//!   on [sec]        — Bật LED (có thể hẹn giờ tắt)
//!   off             — Tắt LED
//!   blink [sec] [ms]— Nhấp nháy LED (hẹn giờ, tốc độ)
//!   toggle [n]      — Đảo LED n lần
//!   breathe         — LED thở bằng PWM (tắt bằng nút BOOT)
//!   touch           — Hiển thị giá trị cảm biến chạm liên tục
//!   benchmark       — Chạy CPU benchmark (đếm số nguyên tố)
//!   wifi            — Bật Wi-Fi AP + Web Server (192.168.4.1)
//!   sleep [sec]     — Vào Deep Sleep (đánh thức bằng nút BOOT)
//!   rand [n]        — Sinh số ngẫu nhiên phần cứng
//!   status          — Xem trạng thái hệ thống
//!
//! Phần cứng: ESP32 DevKit — LED tích hợp GPIO2, nút BOOT GPIO0,
//!            cảm biến chạm GPIO4, UART0 (TX=GPIO1, RX=GPIO3)

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_net::{
    tcp::TcpSocket, Config, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4,
};
use embassy_time::{Duration as EmbassyDuration, Instant as EmbassyInstant, Timer};
use embedded_io_async::Write as AsyncWrite;
use esp_hal::{
    clock::CpuClock,
    gpio::{self, DriveMode, Event, Input, InputConfig, Level, Pull},
    ledc::{
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
        LSGlobalClkSource, LowSpeed, Ledc,
    },
    rng::Rng,
    rtc_cntl::{
        reset_reason, wakeup_cause,
        sleep::{LowPower, RtcSleepConfig},
    },
    system::Cpu,
    time::{Duration, Instant, Rate},
    timer::timg::TimerGroup,
    touch::{Touch, TouchPad},
    uart::{Config as UartConfig, Uart},
};
use esp_backtrace as _;
use esp_hal_dhcp_server::{
    run_dhcp_server,
    structs::{DhcpLease, DhcpLeaser, DhcpServerConfig},
    Ipv4Addr,
};
use esp_println;
use esp_radio::wifi::{
    ap::AccessPointConfig, Config as WifiConfig, ControllerConfig, Interface, WifiController,
};
use log::{error, info};
use static_cell::StaticCell;

// ════════════════════════════════════════════════════════════════
// Panic handler & App descriptor
// ════════════════════════════════════════════════════════════════

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    error!("{}", info);
    loop {}
}

esp_bootloader_esp_idf::esp_app_desc!();

// ════════════════════════════════════════════════════════════════
// Bài 5+: CLI Shell — LED state machine
// ════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Eq)]
enum LedState {
    Off,
    On { expire_at: Option<Instant> },
    Blinking {
        interval: Duration,
        expire_at: Option<Instant>,
        last_toggle: Instant,
    },
}

/// Parse số nguyên đơn giản, hỗ trợ dạng "10", "[10]", "[ 5 ]"
fn parse_number(s: &str) -> Option<u64> {
    let clean = s.trim().trim_matches(|c| c == '[' || c == ']');
    if clean.is_empty() {
        return None;
    }
    let mut val: u64 = 0;
    for b in clean.bytes() {
        if b.is_ascii_digit() {
            val = val.checked_mul(10)?.checked_add((b - b'0') as u64)?;
        } else {
            return None;
        }
    }
    Some(val)
}

// ════════════════════════════════════════════════════════════════
// Bài 3+: CPU Benchmark — đếm số nguyên tố
// ════════════════════════════════════════════════════════════════

fn count_primes(max: u32) -> u32 {
    let mut count = 0;
    for n in 2..=max {
        let mut is_prime = true;
        let mut d = 2;
        while d * d <= n {
            if n % d == 0 {
                is_prime = false;
                break;
            }
            d += 1;
        }
        if is_prime {
            count += 1;
        }
    }
    count
}

// ════════════════════════════════════════════════════════════════
// Bài 6: Wi-Fi — DHCP Leaser thông minh
// ════════════════════════════════════════════════════════════════

struct SmartDhcpLeaser {
    client_ip: Ipv4Addr,
}

impl SmartDhcpLeaser {
    fn new(client_ip: Ipv4Addr) -> Self {
        Self { client_ip }
    }
}

impl DhcpLeaser for SmartDhcpLeaser {
    fn get_lease(&mut self, mac: [u8; 16]) -> Option<DhcpLease> {
        Some(DhcpLease {
            ip: self.client_ip,
            mac,
            expires: EmbassyInstant::now() + EmbassyDuration::from_secs(3600),
        })
    }

    fn next_lease(&mut self) -> Option<Ipv4Addr> {
        Some(self.client_ip)
    }

    fn add_lease(&mut self, ip: Ipv4Addr, _mac: [u8; 16], _expires: EmbassyInstant) -> bool {
        ip == self.client_ip
    }

    fn remove_lease(&mut self, _mac: [u8; 16]) -> bool {
        true
    }
}

// ════════════════════════════════════════════════════════════════
// Embassy Tasks — chạy nền
// ════════════════════════════════════════════════════════════════

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    let mut leaser = SmartDhcpLeaser::new(Ipv4Addr::new(192, 168, 4, 2));
    let config = DhcpServerConfig {
        ip: Ipv4Addr::new(192, 168, 4, 1),
        lease_time: EmbassyDuration::from_secs(3600),
        gateways: &[Ipv4Addr::new(192, 168, 4, 1)],
        subnet: Some(Ipv4Addr::new(255, 255, 255, 0)),
        dns: &[Ipv4Addr::new(192, 168, 4, 1)],
        use_captive_portal: true,
    };
    info!("DHCP Server: auto-assigning 192.168.4.2");
    if let Err(e) = run_dhcp_server(stack, config, &mut leaser).await {
        error!("DHCP Server error: {:?}", e);
    }
}

#[embassy_executor::task]
async fn dns_task(stack: Stack<'static>) {
    use embassy_net::udp::{PacketMetadata, UdpSocket};
    use embassy_net::{IpAddress, IpEndpoint};

    let mut rx_buffer = [0u8; 512];
    let mut tx_buffer = [0u8; 512];
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );

    if let Err(e) = socket.bind(IpEndpoint::new(IpAddress::v4(0, 0, 0, 0), 53)) {
        error!("DNS Bind Error: {:?}", e);
        return;
    }
    info!("DNS Captive Portal on UDP:53");

    let mut buf = [0u8; 512];
    let mut resp = [0u8; 512];

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, meta)) => {
                if n < 12 {
                    continue;
                }
                resp[..n].copy_from_slice(&buf[..n]);
                resp[2] = 0x81;
                resp[3] = 0x80; // Standard response
                resp[6] = 0x00;
                resp[7] = 0x01; // Answer count = 1
                resp[8] = 0x00;
                resp[9] = 0x00;
                resp[10] = 0x00;
                resp[11] = 0x00;

                let idx = n;
                if idx + 16 <= resp.len() {
                    resp[idx] = 0xc0;
                    resp[idx + 1] = 0x0c;
                    resp[idx + 2] = 0x00;
                    resp[idx + 3] = 0x01; // Type A
                    resp[idx + 4] = 0x00;
                    resp[idx + 5] = 0x01; // Class IN
                    resp[idx + 6] = 0x00;
                    resp[idx + 7] = 0x00;
                    resp[idx + 8] = 0x00;
                    resp[idx + 9] = 0x3c; // TTL 60s
                    resp[idx + 10] = 0x00;
                    resp[idx + 11] = 0x04;
                    resp[idx + 12] = 192;
                    resp[idx + 13] = 168;
                    resp[idx + 14] = 4;
                    resp[idx + 15] = 1; // → 192.168.4.1
                    let _ = socket.send_to(&resp[..idx + 16], meta.endpoint).await;
                }
            }
            Err(_) => {}
        }
    }
}

// ════════════════════════════════════════════════════════════════
// Biến chia sẻ giữa Web Server và Main Loop (Thread-safe Atomics)
// ════════════════════════════════════════════════════════════════

static WEB_CMD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
// 0: None, 1: On, 2: Off, 3: Toggle, 4: Blink 5s, 5: Breathe, 6: Touch Toggle, 7: Rand, 8: Benchmark, 9: Wifi Off

static SYS_TOUCH_VAL: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(410);
static SYS_TOUCH_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
static SYS_LED_STATUS: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0); // 0: OFF, 1: ON, 2: BLINK, 3: BREATHE
static SYS_LAST_RAND: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static SYS_BENCHMARK_MS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static SYS_BENCHMARK_PRIMES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static SYS_UPTIME_SECS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static SYS_WIFI_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[embassy_executor::task]
async fn web_server_task(stack: Stack<'static>) -> ! {
    let mut rx_buffer = [0u8; 1536];
    let mut tx_buffer = [0u8; 1536];

    info!("Web Server listening on port 80");

    static STACK_REF: StaticCell<Stack<'static>> = StaticCell::new();
    let stack_ref = STACK_REF.init(stack);

    loop {
        let mut socket = TcpSocket::new(*stack_ref, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(EmbassyDuration::from_secs(10)));

        if let Err(e) = socket.accept(80).await {
            info!("Accept error: {:?}", e);
            continue;
        }

        let mut req_buf = [0u8; 1024];
        let n = match socket.read(&mut req_buf).await {
            Ok(0) => {
                socket.close();
                continue;
            }
            Ok(n) => n,
            Err(_) => {
                socket.close();
                continue;
            }
        };

        let request = core::str::from_utf8(&req_buf[..n]).unwrap_or("");

        // Xử lý các lệnh điều khiển từ Web Dashboard
        let action = if request.contains("GET /led/on") {
            WEB_CMD.store(1, core::sync::atomic::Ordering::Relaxed);
            "BẬT LED"
        } else if request.contains("GET /led/off") {
            WEB_CMD.store(2, core::sync::atomic::Ordering::Relaxed);
            "TẮT LED"
        } else if request.contains("GET /led/toggle") {
            WEB_CMD.store(3, core::sync::atomic::Ordering::Relaxed);
            "ĐẢO LED"
        } else if request.contains("GET /led/blink") {
            WEB_CMD.store(4, core::sync::atomic::Ordering::Relaxed);
            "BLINK 5S"
        } else if request.contains("GET /led/breathe") {
            WEB_CMD.store(5, core::sync::atomic::Ordering::Relaxed);
            "BREATHE LED"
        } else if request.contains("GET /touch/toggle") {
            WEB_CMD.store(6, core::sync::atomic::Ordering::Relaxed);
            "ĐẢO GIÁM SÁT CHẠM"
        } else if request.contains("GET /rand") {
            WEB_CMD.store(7, core::sync::atomic::Ordering::Relaxed);
            "SINH SỐ NGẪU NHIÊN"
        } else if request.contains("GET /benchmark") {
            WEB_CMD.store(8, core::sync::atomic::Ordering::Relaxed);
            "BENCHMARK"
        } else if request.contains("GET /wifi/off") {
            WEB_CMD.store(9, core::sync::atomic::Ordering::Relaxed);
            "TẮT WI-FI"
        } else {
            "XEM TRANG"
        };

        info!("HTTP: {} (lệnh={})", &request[..request.len().min(35)], action);

        // Đọc dữ liệu telemetry hệ thống để hiển thị
        let uptime = SYS_UPTIME_SECS.load(core::sync::atomic::Ordering::Relaxed);
        let touch_val = SYS_TOUCH_VAL.load(core::sync::atomic::Ordering::Relaxed);
        let touch_active = SYS_TOUCH_ACTIVE.load(core::sync::atomic::Ordering::Relaxed);
        let led_st = SYS_LED_STATUS.load(core::sync::atomic::Ordering::Relaxed);
        let last_rand = SYS_LAST_RAND.load(core::sync::atomic::Ordering::Relaxed);
        let bench_ms = SYS_BENCHMARK_MS.load(core::sync::atomic::Ordering::Relaxed);
        let bench_primes = SYS_BENCHMARK_PRIMES.load(core::sync::atomic::Ordering::Relaxed);

        let led_text = match led_st {
            1 => "<span style='color:#22c55e;font-weight:bold'>ĐANG BẬT</span>",
            2 => "<span style='color:#f59e0b;font-weight:bold'>NHẤP NHÁY (BLINK)</span>",
            3 => "<span style='color:#a855f7;font-weight:bold'>THỞ PWM (BREATHE)</span>",
            _ => "<span style='color:#94a3b8;font-weight:bold'>ĐANG TẮT</span>",
        };

        let touch_text = if touch_active {
            "<span style='color:#22c55e;font-weight:bold'>BẬT</span>"
        } else {
            "<span style='color:#ef4444;font-weight:bold'>TẮT</span>"
        };

        let html = alloc::format!(
            "<!DOCTYPE html><html><head><meta charset='utf-8'>\
            <meta name='viewport' content='width=device-width,initial-scale=1'>\
            <title>ESP32 Flagship Dashboard</title>\
            <style>\
            body{{font-family:system-ui,-apple-system,sans-serif;background:#0f172a;color:#f8fafc;margin:0;padding:1rem;display:flex;flex-direction:column;align-items:center}}\
            .container{{width:100%;max-width:440px}}\
            .header{{text-align:center;margin-bottom:1.2rem}}\
            h1{{font-size:1.4rem;margin:0.2rem 0;color:#38bdf8}}\
            .subtitle{{font-size:0.85rem;color:#94a3b8;margin:0}}\
            .card{{background:#1e293b;border-radius:0.75rem;padding:1.2rem;margin-bottom:1rem;box-shadow:0 4px 12px rgba(0,0,0,0.3);border:1px solid #334155}}\
            .card-title{{font-size:1rem;font-weight:700;color:#e2e8f0;margin:0 0 0.8rem 0}}\
            .stat-grid{{display:grid;grid-template-columns:1fr 1fr;gap:0.6rem;font-size:0.85rem}}\
            .stat-box{{background:#0f172a;padding:0.6rem;border-radius:0.5rem;border:1px solid #1e293b}}\
            .stat-label{{color:#94a3b8;font-size:0.75rem}}\
            .stat-val{{font-size:0.95rem;margin-top:0.2rem}}\
            .btn-grid{{display:grid;grid-template-columns:1fr 1fr;gap:0.6rem}}\
            .btn{{display:flex;align-items:center;justify-content:center;padding:0.75rem 0.5rem;border:none;border-radius:0.5rem;font-size:0.9rem;font-weight:600;text-decoration:none;color:#fff;text-align:center;box-sizing:border-box;transition:all 0.15s ease}}\
            .btn:active{{transform:scale(0.97)}}\
            .btn-full{{grid-column:1 / -1}}\
            .btn-green{{background:#16a34a}}\
            .btn-red{{background:#dc2626}}\
            .btn-blue{{background:#2563eb}}\
            .btn-amber{{background:#d97706}}\
            .btn-purple{{background:#9333ea}}\
            .btn-cyan{{background:#0891b2}}\
            .btn-pink{{background:#db2777}}\
            .btn-danger{{background:#7f1d1d;color:#fca5a5;border:1px solid #991b1b}}\
            .refresh-bar{{text-align:center;margin-top:0.8rem}}\
            .refresh-link{{color:#38bdf8;text-decoration:none;font-size:0.85rem}}\
            </style></head><body>\
            <div class='container'>\
            <div class='header'>\
                <h1>⚡ ESP32 RUST FLAGSHIP</h1>\
                <p class='subtitle'>Bảng điều khiển Web SoftAP (192.168.4.1)</p>\
            </div>\
            <div class='card'>\
                <div class='card-title'>📊 Trạng Thái Hệ Thống</div>\
                <div class='stat-grid'>\
                    <div class='stat-box'><div class='stat-label'>Thời gian chạy</div><div class='stat-val'>{}s</div></div>\
                    <div class='stat-box'><div class='stat-label'>Trạng thái LED</div><div class='stat-val'>{}</div></div>\
                    <div class='stat-box'><div class='stat-label'>Điện dung GPIO4</div><div class='stat-val'>{} ({})</div></div>\
                    <div class='stat-box'><div class='stat-label'>Số ngẫu nhiên RNG</div><div class='stat-val'>{}</div></div>\
                    <div class='stat-box' style='grid-column:1/-1'><div class='stat-label'>Benchmark CPU</div><div class='stat-val'>{} ms ({} số nguyên tố)</div></div>\
                </div>\
            </div>\
            <div class='card'>\
                <div class='card-title'>💡 Điều Khiển Đèn LED & PWM</div>\
                <div class='btn-grid'>\
                    <a class='btn btn-green' href='/led/on'>BẬT LED</a>\
                    <a class='btn btn-red' href='/led/off'>TẮT LED</a>\
                    <a class='btn btn-blue btn-full' href='/led/toggle'>ĐẢO TRẠNG THÁI LED</a>\
                    <a class='btn btn-amber' href='/led/blink'>NHẤP NHÁY 5S</a>\
                    <a class='btn btn-purple' href='/led/breathe'>LED THỞ PWM</a>\
                </div>\
            </div>\
            <div class='card'>\
                <div class='card-title'>⚙️ Cảm Biến & Tiện Ích</div>\
                <div class='btn-grid'>\
                    <a class='btn btn-cyan btn-full' href='/touch/toggle'>BẬT/TẮT GIÁM SÁT CHẠM</a>\
                    <a class='btn btn-pink' href='/rand'>SINH SỐ RNG</a>\
                    <a class='btn btn-amber' href='/benchmark'>BENCHMARK CPU</a>\
                    <a class='btn btn-danger btn-full' href='/wifi/off'>TẮT SÓNG WI-FI SOFTAP</a>\
                </div>\
            </div>\
            <div class='refresh-bar'>\
                <a class='refresh-link' href='/'>🔄 Làm mới dữ liệu</a>\
            </div>\
            </div></body></html>",
            uptime,
            led_text,
            touch_val,
            touch_text,
            last_rand,
            bench_ms,
            bench_primes
        );

        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n").await;
        let _ = socket.write_all(html.as_bytes()).await;
        let _ = socket.flush().await;
        socket.close();
    }
}

// ════════════════════════════════════════════════════════════════
// Điều khiển LED kép: GPIO2 (Onboard) + GPIO15 (Ngoài) qua PWM
// ════════════════════════════════════════════════════════════════

struct DualLed<'a> {
    onboard: channel::Channel<'a, LowSpeed>,
    ext: channel::Channel<'a, LowSpeed>,
    is_on: bool,
}

impl<'a> DualLed<'a> {
    fn new(onboard: channel::Channel<'a, LowSpeed>, ext: channel::Channel<'a, LowSpeed>) -> Self {
        Self {
            onboard,
            ext,
            is_on: false,
        }
    }

    fn set_high(&mut self) {
        let _ = self.onboard.set_duty(100);
        let _ = self.ext.set_duty(100);
        self.is_on = true;
    }

    fn set_low(&mut self) {
        let _ = self.onboard.set_duty(0);
        let _ = self.ext.set_duty(0);
        self.is_on = false;
    }

    fn set_level(&mut self, level: Level) {
        match level {
            Level::High => self.set_high(),
            Level::Low => self.set_low(),
        }
    }

    fn toggle(&mut self) {
        if self.is_on {
            self.set_low();
        } else {
            self.set_high();
        }
    }

    fn start_fade(&mut self, start: u8, end: u8, duration_ms: u16) {
        // set_duty and start_duty_fade expect percentage values (0-100)
        let _ = self.onboard.set_duty(start);
        let _ = self.ext.set_duty(start);
        let _ = self.onboard.start_duty_fade(start, end, duration_ms);
        let _ = self.ext.start_duty_fade(start, end, duration_ms);
    }

    fn is_fade_running(&self) -> bool {
        self.onboard.is_duty_fade_running() || self.ext.is_duty_fade_running()
    }
}

// ════════════════════════════════════════════════════════════════
// MAIN — Entry point tổng hợp
// ════════════════════════════════════════════════════════════════

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // Logger must be initialized before everything else
    esp_println::logger::init_logger_from_env();

    info!("╔══════════════════════════════════════════════════╗");
    info!("║      ESP32 RUST FLAGSHIP — DỰ ÁN TỔNG HỢP      ║");
    info!("╚══════════════════════════════════════════════════╝");

    // ── Bài 8: Phân tích nguyên nhân khởi động ──
    let reason = reset_reason(Cpu::ProCpu);
    info!("Reset reason: {:?}", reason);
    let wakeup = wakeup_cause();
    for source in wakeup.iter() {
        info!("Wakeup source: {:?}", source);
    }

    // ── Khởi tạo phần cứng ──
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Heap cho Wi-Fi stack
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 98768);

    // Embassy timer
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Button & RNG
    let mut button = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );
    let rng = Rng::new();

    // Wi-Fi peripheral — wrapped in Option so we only take it once
    let mut wifi_peripheral = Some(peripherals.WIFI);

    // UART0 cho CLI Shell
    let mut uart0 = Uart::new(peripherals.UART0, UartConfig::default())
        .unwrap()
        .with_rx(peripherals.GPIO3)
        .with_tx(peripherals.GPIO1);

    // Touch sensor (GPIO4)
    let touch_controller = Touch::continuous_mode(peripherals.TOUCH, None);
    let mut touch_pad = TouchPad::new(peripherals.GPIO4, &touch_controller);

    // LEDC PWM: Cấu hình Channel0 trên GPIO2 (LED Onboard) & Channel1 trên GPIO15 (LED ngoài)
    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
    let mut lstimer0 = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    lstimer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(24),
        })
        .unwrap();

    let mut led_onboard = ledc.channel(channel::Number::Channel0, peripherals.GPIO2);
    led_onboard
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .unwrap();

    let mut led_ext = ledc.channel(channel::Number::Channel1, peripherals.GPIO15);
    led_ext
        .configure(channel::config::Config {
            timer: &lstimer0,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .unwrap();

    let mut led = DualLed::new(led_onboard, led_ext);

    info!("Hardware initialized: Onboard LED(GPIO2 PWM), Ext LED(GPIO15 PWM), Button(GPIO0), Touch(GPIO4)");

    // ── Banner CLI Shell ──
    let _ = write!(
        uart0,
        "\r\n\
         ╔══════════════════════════════════════════════════╗\r\n\
         ║      ESP32 RUST FLAGSHIP — CLI Shell             ║\r\n\
         ╠══════════════════════════════════════════════════╣\r\n\
         ║  Gõ 'help' để xem danh sách lệnh                 ║\r\n\
         ║  Cảm biến chạm GPIO4: ĐÃ KÍCH HOẠT SẴN           ║\r\n\
         ╚══════════════════════════════════════════════════╝\r\n\r\n> "
    );

    // ── State ──
    let mut led_state = LedState::Off;
    let boot_time = Instant::now();
    let mut buf = [0u8; 64];
    let mut idx = 0;
    let mut rx_buf = [0u8; 16];
    let mut wifi_started = false;
    let mut wifi_active = false;
    let mut esc_state = 0u8;

    // Trạng thái cảm biến chạm (Mặc định BẬT SẴN để chạm vào GPIO4 là sáng đèn ngay)
    let mut touch_monitor_active = true;
    let mut touch_is_pressed = false;
    // Khởi tạo baseline bằng đọc thực tế từ cảm biến
    let mut touch_baseline: u16 = touch_pad.read();
    let _ = write!(uart0, "\r\n-> Touch baseline khởi tạo: {}\r\n", touch_baseline);
    let mut last_touch_check = Instant::now();

    // ── Main loop: CLI Shell + LED state machine ──
    loop {
        let now = Instant::now();
        SYS_UPTIME_SECS.store(boot_time.elapsed().as_secs() as u32, core::sync::atomic::Ordering::Relaxed);

        // ─── 0. Nhận lệnh điều khiển từ Web Server Dashboard ───
        let web_cmd = WEB_CMD.load(core::sync::atomic::Ordering::Relaxed);
        if web_cmd != 0 {
            WEB_CMD.store(0, core::sync::atomic::Ordering::Relaxed);
            let current_input = core::str::from_utf8(&buf[..idx]).unwrap_or("");
            match web_cmd {
                1 => {
                    led_state = LedState::On { expire_at: None };
                    SYS_LED_STATUS.store(1, core::sync::atomic::Ordering::Relaxed);
                    let _ = write!(uart0, "\r\n-> [Web Server] Nhận lệnh: BẬT LED\r\n> {}", current_input);
                }
                2 => {
                    led_state = LedState::Off;
                    SYS_LED_STATUS.store(0, core::sync::atomic::Ordering::Relaxed);
                    let _ = write!(uart0, "\r\n-> [Web Server] Nhận lệnh: TẮT LED\r\n> {}", current_input);
                }
                3 => {
                    led.toggle();
                    let st = if led.is_on { 1 } else { 0 };
                    SYS_LED_STATUS.store(st, core::sync::atomic::Ordering::Relaxed);
                    let _ = write!(uart0, "\r\n-> [Web Server] Nhận lệnh: ĐẢO TRẠNG THÁI LED\r\n> {}", current_input);
                }
                4 => {
                    let expire = now + Duration::from_secs(5);
                    led_state = LedState::Blinking {
                        interval: Duration::from_millis(500),
                        expire_at: Some(expire),
                        last_toggle: now,
                    };
                    SYS_LED_STATUS.store(2, core::sync::atomic::Ordering::Relaxed);
                    let _ = write!(uart0, "\r\n-> [Web Server] Nhận lệnh: BLINK 5 GIÂY\r\n> {}", current_input);
                }
                5 => {
                    let _ = write!(uart0, "\r\n-> [Web Server] Nhận lệnh: LED THỞ PWM (1 chu kỳ)\r\n> {}", current_input);
                    SYS_LED_STATUS.store(3, core::sync::atomic::Ordering::Relaxed);
                    led.start_fade(0, 100, 1000);
                    while led.is_fade_running() {
                        Timer::after(EmbassyDuration::from_millis(15)).await;
                    }
                    led.start_fade(100, 0, 1000);
                    while led.is_fade_running() {
                        Timer::after(EmbassyDuration::from_millis(15)).await;
                    }
                    SYS_LED_STATUS.store(0, core::sync::atomic::Ordering::Relaxed);
                }
                6 => {
                    touch_monitor_active = !touch_monitor_active;
                    SYS_TOUCH_ACTIVE.store(touch_monitor_active, core::sync::atomic::Ordering::Relaxed);
                    let _ = write!(
                        uart0,
                        "\r\n-> [Web Server] Nhận lệnh: ĐÃ {} GIÁM SÁT CHẠM GPIO4\r\n> {}",
                        if touch_monitor_active { "BẬT" } else { "TẮT" },
                        current_input
                    );
                }
                7 => {
                    let r = rng.random();
                    SYS_LAST_RAND.store(r, core::sync::atomic::Ordering::Relaxed);
                    let _ = write!(uart0, "\r\n-> [Web Server] Nhận lệnh: SINH SỐ NGẪU NHIÊN: {}\r\n> {}", r, current_input);
                }
                8 => {
                    let _ = write!(uart0, "\r\n-> [Web Server] Nhận lệnh: CHẠY CPU BENCHMARK...\r\n> {}", current_input);
                    let start = Instant::now();
                    let primes = count_primes(100_000);
                    let elapsed = start.elapsed().as_millis() as u32;
                    SYS_BENCHMARK_MS.store(elapsed, core::sync::atomic::Ordering::Relaxed);
                    SYS_BENCHMARK_PRIMES.store(primes as u32, core::sync::atomic::Ordering::Relaxed);
                    let _ = write!(uart0, "-> Benchmark hoàn tất: {} ms ({} số nguyên tố)\r\n> {}", elapsed, primes, current_input);
                }
                9 => {
                    let _ = write!(uart0, "\r\n-> [Web Server] Nhận lệnh: TẮT SÓNG WI-FI SOFTAP\r\n> {}", current_input);
                    unsafe { esp_wifi_sys_esp32::include::esp_wifi_stop(); }
                    wifi_active = false;
                    SYS_WIFI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
                    led.set_low();
                }
                _ => {}
            }
        }

        // ─── 0.1. Kiểm tra nút BOOT (GPIO0) bấm đè >= 1.5s để Bật/Tắt Wi-Fi ───
        if button.is_low() {
            let press_start = Instant::now();
            let mut triggered = false;
            while button.is_low() {
                if !triggered && press_start.elapsed() >= Duration::from_millis(1500) {
                    triggered = true;
                    // BẬT / TẮT WI-FI SOFTAP
                    if !wifi_started {
                        let ap_config = WifiConfig::AccessPoint(
                            AccessPointConfig::default()
                                .with_ssid("ESP32-Rust-WiFi".try_into().unwrap())
                                .with_authentication(
                                    esp_radio::wifi::AuthenticationMethodConfig::Wpa2Personal(
                                        "12345678".try_into().unwrap(),
                                    ),
                                ),
                        );
                        let controller_config = ControllerConfig::default().with_initial_config(ap_config);
                        let wifi_peri = wifi_peripheral.take().expect("Wi-Fi peripheral already used");
                        let mut wifi_controller = WifiController::new(wifi_peri, controller_config).expect("Wi-Fi init failed");
                        let _ = wifi_controller.set_max_tx_power(60);

                        let wifi_interface = Interface::access_point();
                        let net_config = Config::ipv4_static(StaticConfigV4 {
                            address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 4, 1), 24),
                            gateway: Some(Ipv4Address::new(192, 168, 4, 1)),
                            dns_servers: Default::default(),
                        });

                        static STACK_RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
                        let resources = STACK_RESOURCES.init(StackResources::new());
                        let (stack, runner) = embassy_net::new(wifi_interface, net_config, resources, 12345);

                        spawner.spawn(net_task(runner).unwrap());
                        spawner.spawn(dhcp_task(stack).unwrap());
                        spawner.spawn(dns_task(stack).unwrap());
                        spawner.spawn(web_server_task(stack).unwrap());

                        core::mem::forget(wifi_controller);

                        wifi_started = true;
                        wifi_active = true;
                        SYS_WIFI_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);

                        // Chớp LED onboard 3 lần nhanh báo hiệu BẬT
                        for _ in 0..3 {
                            led.set_high();
                            let t = Instant::now();
                            while t.elapsed() < Duration::from_millis(80) {}
                            led.set_low();
                            let t = Instant::now();
                            while t.elapsed() < Duration::from_millis(80) {}
                        }
                        // Sync led_state after blinking
                        led_state = LedState::Off;

                        let current_input = core::str::from_utf8(&buf[..idx]).unwrap_or("");
                        let _ = write!(
                            uart0,
                            "\r\n==================================================\r\n\
                             ⚡ [Nút BOOT đè 1.5s] ĐÃ BẬT Wi-Fi SoftAP!\r\n\
                             SSID: ESP32-Rust-WiFi | Pass: 12345678\r\n\
                             Web UI: http://192.168.4.1\r\n\
                            ==================================================\r\n> {}",
                            current_input
                        );
                    } else if wifi_active {
                        unsafe { esp_wifi_sys_esp32::include::esp_wifi_stop(); }
                        wifi_active = false;
                        SYS_WIFI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);

                        // Chớp LED onboard 1 lần dài báo hiệu TẮT
                        led.set_high();
                        let t = Instant::now();
                        while t.elapsed() < Duration::from_millis(400) {}
                        led.set_low();
                        // Sync led_state after blinking
                        led_state = LedState::Off;

                        let current_input = core::str::from_utf8(&buf[..idx]).unwrap_or("");
                        let _ = write!(
                            uart0,
                            "\r\n🍃 [Nút BOOT đè 1.5s] ĐÃ TẮT Wi-Fi SoftAP (Tắt sóng Radio).\r\n> {}",
                            current_input
                        );
                    } else {
                        unsafe { esp_wifi_sys_esp32::include::esp_wifi_start(); }
                        wifi_active = true;
                        SYS_WIFI_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);

                        // Chớp LED onboard 3 lần nhanh báo hiệu BẬT LẠI
                        for _ in 0..3 {
                            led.set_high();
                            let t = Instant::now();
                            while t.elapsed() < Duration::from_millis(80) {}
                            led.set_low();
                            let t = Instant::now();
                            while t.elapsed() < Duration::from_millis(80) {}
                        }
                        // Sync led_state after blinking
                        led_state = LedState::Off;

                        let current_input = core::str::from_utf8(&buf[..idx]).unwrap_or("");
                        let _ = write!(
                            uart0,
                            "\r\n⚡ [Nút BOOT đè 1.5s] ĐÃ BẬT LẠI Wi-Fi SoftAP!\r\n\
                             Web UI: http://192.168.4.1\r\n> {}",
                            current_input
                        );
                    }
                }
                Timer::after(EmbassyDuration::from_millis(20)).await;
            }
        }

        // ─── 1. Giám sát cảm biến chạm nền (Tự động phát hiện & sáng LED onboard) ───
        if last_touch_check.elapsed() >= Duration::from_millis(50) {
            last_touch_check = now;
            if let Some(current_val) = touch_pad.try_read() {
                SYS_TOUCH_VAL.store(current_val, core::sync::atomic::Ordering::Relaxed);
                if touch_monitor_active {
                    // Debug: in giá trị touch định kỳ (chỉ khi có thay đổi lớn)
                    static mut LAST_LOGGED_VAL: u16 = 0;
                    unsafe {
                        if current_val.abs_diff(LAST_LOGGED_VAL) > 20 {
                            let _ = write!(uart0, "\r\n[DEBUG] Touch val: {}, baseline: {}, pressed: {}\r\n> {}", current_val, touch_baseline, touch_is_pressed, core::str::from_utf8(&buf[..idx]).unwrap_or(""));
                            LAST_LOGGED_VAL = current_val;
                        }
                    }
                    if !touch_is_pressed {
                        // Cập nhật baseline động khi không chạm
                        if current_val >= 350 {
                            touch_baseline = current_val;
                        } else if current_val < 250 {
                            // PHÁT HIỆN CHẠM -> BẬT ĐÈN ONBOARD NGAY!
                            touch_is_pressed = true;
                            let delta = touch_baseline.saturating_sub(current_val);
                            led.set_high();
                            SYS_LED_STATUS.store(1, core::sync::atomic::Ordering::Relaxed);
                            led_state = LedState::On {
                                expire_at: None, // Vĩnh viễn cho đến khi buông tay
                            };
                            let current_input = core::str::from_utf8(&buf[..idx]).unwrap_or("");
                            let _ = write!(
                                uart0,
                                "\r\n⚡ [CHẠM GPIO4] Điện dung: {} -> {} (Giảm {}). Đèn LED onboard ĐÃ BẬT!\r\n> {}",
                                touch_baseline, current_val, delta, current_input
                            );
                        }
                    } else if current_val >= 320 {
                        // BUÔNG TAY -> TẮT ĐÈN ONBOARD
                        touch_is_pressed = false;
                        led.set_low();
                        SYS_LED_STATUS.store(0, core::sync::atomic::Ordering::Relaxed);
                        led_state = LedState::Off;
                        // Cập nhật baseline mới
                        touch_baseline = current_val;
                        let current_input = core::str::from_utf8(&buf[..idx]).unwrap_or("");
                        let _ = write!(
                            uart0,
                            "\r\n🍃 [BUÔNG TAY] Buông tay khỏi GPIO4. Điện dung hồi phục: {}. Đèn LED onboard ĐÃ TẮT.\r\n> {}",
                            current_val, current_input
                        );
                    }
                }
            }
        }

        // ─── 2. Cập nhật trạng thái LED (non-blocking) ───
        match led_state {
            LedState::Off => {
                led.set_level(Level::Low);
                SYS_LED_STATUS.store(0, core::sync::atomic::Ordering::Relaxed);
            }
            LedState::On { expire_at } => {
                led.set_level(Level::High);
                SYS_LED_STATUS.store(1, core::sync::atomic::Ordering::Relaxed);
                if let Some(deadline) = expire_at {
                    if now >= deadline {
                        led_state = LedState::Off;
                        SYS_LED_STATUS.store(0, core::sync::atomic::Ordering::Relaxed);
                        let current_input = core::str::from_utf8(&buf[..idx]).unwrap_or("");
                        let _ = write!(uart0, "\r\n-> [Timer] LED tự tắt.\r\n> {}", current_input);
                    }
                }
            }
            LedState::Blinking {
                interval,
                expire_at,
                last_toggle,
            } => {
                SYS_LED_STATUS.store(2, core::sync::atomic::Ordering::Relaxed);
                if let Some(deadline) = expire_at {
                    if now >= deadline {
                        led_state = LedState::Off;
                        SYS_LED_STATUS.store(0, core::sync::atomic::Ordering::Relaxed);
                        let current_input = core::str::from_utf8(&buf[..idx]).unwrap_or("");
                        let _ = write!(uart0, "\r\n-> [Timer] Blink kết thúc.\r\n> {}", current_input);
                    }
                }
                if let LedState::Blinking { .. } = led_state {
                    if last_toggle.elapsed() >= interval {
                        led.toggle();
                        led_state = LedState::Blinking {
                            interval,
                            expire_at,
                            last_toggle: now,
                        };
                    }
                }
            }
        }

        // ─── 3. Đọc phím từ UART (non-blocking) ───
        if let Ok(count) = uart0.read_buffered(&mut rx_buf) {
            for &ch in &rx_buf[..count] {
                // ANSI / VT100 Escape Sequence Filter (Lọc phím mũi tên Up/Down/Left/Right, Delete, v.v.)
                if esc_state == 1 {
                    if ch == b'[' || ch == b'O' {
                        esc_state = 2;
                    } else {
                        esc_state = 0;
                    }
                    continue;
                } else if esc_state == 2 {
                    if (b'0'..=b'9').contains(&ch) || ch == b';' {
                        esc_state = 3;
                    } else {
                        esc_state = 0;
                    }
                    continue;
                } else if esc_state == 3 {
                    if ch == b'~' || (b'A'..=b'Z').contains(&ch) || (b'a'..=b'z').contains(&ch) {
                        esc_state = 0;
                    }
                    continue;
                }

                if ch == 0x1B {
                    esc_state = 1;
                    continue;
                }

                if ch == b'\r' || ch == b'\n' {
                    let _ = write!(uart0, "\r\n");

                    if idx > 0 {
                        if let Ok(cmd_str) = core::str::from_utf8(&buf[..idx]) {
                            let mut parts = cmd_str.trim().split_ascii_whitespace();
                            let cmd = parts.next().unwrap_or("");
                            let arg1 = parts.next();
                            let arg2 = parts.next();

                            match cmd {
                                // ═══ Bài 1: Bật LED ═══
                                c if c.eq_ignore_ascii_case("on") => {
                                    if let Some(sec_str) = arg1 {
                                        if let Some(sec) = parse_number(sec_str) {
                                            let expire = now + Duration::from_secs(sec);
                                            led_state = LedState::On { expire_at: Some(expire) };
                                            let _ = write!(uart0, "-> LED ON trong {} giây.\r\n", sec);
                                        } else {
                                            let _ = write!(uart0, "-> Sai tham số. Ví dụ: on 5\r\n");
                                        }
                                    } else {
                                        led_state = LedState::On { expire_at: None };
                                        let _ = write!(uart0, "-> LED ON vĩnh viễn.\r\n");
                                    }
                                }

                                // ═══ Bài 1: Tắt LED ═══
                                c if c.eq_ignore_ascii_case("off") => {
                                    led_state = LedState::Off;
                                    let _ = write!(uart0, "-> LED OFF.\r\n");
                                }

                                // ═══ Bài 1: Nhấp nháy LED ═══
                                c if c.eq_ignore_ascii_case("blink") => {
                                    let sec_opt = arg1.and_then(parse_number);
                                    let speed_ms = arg2.and_then(parse_number).unwrap_or(500);
                                    let interval = Duration::from_millis(speed_ms);

                                    if let Some(sec) = sec_opt {
                                        let expire = now + Duration::from_secs(sec);
                                        led_state = LedState::Blinking {
                                            interval,
                                            expire_at: Some(expire),
                                            last_toggle: now,
                                        };
                                        let _ = write!(uart0, "-> Blink {} giây (tốc độ {}ms).\r\n", sec, speed_ms);
                                    } else {
                                        led_state = LedState::Blinking {
                                            interval,
                                            expire_at: None,
                                            last_toggle: now,
                                        };
                                        let _ = write!(uart0, "-> Blink vĩnh viễn ({}ms). Gõ 'off' để dừng.\r\n", speed_ms);
                                    }
                                }

                                // ═══ Bài 1: Toggle LED ═══
                                c if c.eq_ignore_ascii_case("toggle") => {
                                    let times = arg1.and_then(parse_number).unwrap_or(1);
                                    for _ in 0..times {
                                        led.toggle();
                                        let t = Instant::now();
                                        while t.elapsed() < Duration::from_millis(150) {}
                                    }
                                    let _ = write!(uart0, "-> Toggle {} lần.\r\n", times);
                                }

                                // ═══ Bài 2: LED thở bằng PWM ═══
                                c if c.eq_ignore_ascii_case("breathe") || c.eq_ignore_ascii_case("breath") => {
                                    // Xả sạch mọi byte còn dư trong UART RX buffer (tránh phím Enter thừa làm dừng ngay lập tức)
                                    let mut flush_buf = [0u8; 32];
                                    while let Ok(n) = uart0.read_buffered(&mut flush_buf) {
                                        if n == 0 { break; }
                                    }

                                    let _ = write!(
                                        uart0,
                                        "-> Breathing LED (GPIO2 Onboard + GPIO15 Ngoài). Nhấn phím bất kỳ hoặc nút BOOT để dừng...\r\n"
                                    );
                                    let mut stopped = false;
                                    loop {
                                        if stopped { break; }
                                        // Sáng dần 0% -> 100% trong 1000ms
                                        led.start_fade(0, 100, 1000);
                                        while led.is_fade_running() {
                                            let mut key_buf = [0u8; 8];
                                            let key_hit = uart0.read_buffered(&mut key_buf).map(|n| n > 0).unwrap_or(false);
                                            if button.is_low() || key_hit {
                                                led.set_low();
                                                let _ = write!(uart0, "-> Breathing dừng.\r\n");
                                                while button.is_low() {}
                                                stopped = true;
                                                break;
                                            }
                                            Timer::after(EmbassyDuration::from_millis(15)).await;
                                        }
                                        if stopped { break; }
                                        // Tối dần 100% -> 0% trong 1000ms
                                        led.start_fade(100, 0, 1000);
                                        while led.is_fade_running() {
                                            let mut key_buf = [0u8; 8];
                                            let key_hit = uart0.read_buffered(&mut key_buf).map(|n| n > 0).unwrap_or(false);
                                            if button.is_low() || key_hit {
                                                led.set_low();
                                                let _ = write!(uart0, "-> Breathing dừng.\r\n");
                                                while button.is_low() {}
                                                stopped = true;
                                                break;
                                            }
                                            Timer::after(EmbassyDuration::from_millis(15)).await;
                                        }
                                    }
                                }

                                // ═══ Bài 4: Cảm biến chạm kích hoạt tự đánh thức ═══
                                c if c.eq_ignore_ascii_case("touch") => {
                                    match arg1 {
                                        Some(s) if s.eq_ignore_ascii_case("on") => {
                                            touch_baseline = touch_pad.read();
                                            touch_monitor_active = true;
                                            touch_is_pressed = false;
                                            let _ = write!(
                                                uart0,
                                                "-> ĐÃ KÍCH HOẠT giám sát chạm nền GPIO4!\r\n\
                                                   Điện dung cơ sở: {}\r\n\
                                                   Hệ thống sẽ tự động phát hiện, đánh thức LED và gửi cảnh báo khi có tiếp xúc.\r\n",
                                                touch_baseline
                                            );
                                        }
                                        Some(s) if s.eq_ignore_ascii_case("off") => {
                                            touch_monitor_active = false;
                                            touch_is_pressed = false;
                                            let _ = write!(uart0, "-> Đã tắt tính năng giám sát chạm GPIO4.\r\n");
                                        }
                                        Some(s) if s.eq_ignore_ascii_case("watch") || s.eq_ignore_ascii_case("wait") => {
                                            let baseline = touch_pad.read();
                                            let _ = write!(
                                                uart0,
                                                "╔════════════════════════════════════════════════════╗\r\n\
                                                 ║  CHẾ ĐỘ CHỜ CHẠM ĐÁNH THỨC (STANDBY WAKEUP)        ║\r\n\
                                                 ╠════════════════════════════════════════════════════╣\r\n\
                                                 ║  Điện dung cơ sở : {:<31}║\r\n\
                                                 ║  Ngưỡng kích hoạt: < 250 (giảm > 100 đơn vị)      ║\r\n\
                                                 ║  >>> CHẠM VÀO CHÂN GPIO4 ĐỂ ĐÁNH THỨC ESP32! <<<  ║\r\n\
                                                 ║  (Nhấn nút BOOT hoặc gửi phím bất kỳ để thoát)     ║\r\n\
                                                 ╚════════════════════════════════════════════════════╝\r\n",
                                                baseline
                                            );
                                            let mut waiting_touch = false;
                                            loop {
                                                // Kiểm tra phím UART để thoát
                                                let mut exit_buf = [0u8; 4];
                                                if let Ok(n) = uart0.read_buffered(&mut exit_buf) {
                                                    if n > 0 {
                                                        let _ = write!(uart0, "-> Đã nhận phím: Thoát chế độ chờ chạm.\r\n");
                                                        break;
                                                    }
                                                }
                                                // Kiểm tra nút BOOT
                                                if button.is_low() {
                                                    while button.is_low() {}
                                                    let _ = write!(uart0, "-> Đã bấm nút BOOT: Thoát chế độ chờ chạm.\r\n");
                                                    break;
                                                }

                                                let cur = touch_pad.read();
                                                if !waiting_touch && cur < 250 {
                                                    waiting_touch = true;
                                                    let delta = baseline.saturating_sub(cur);
                                                    let _ = write!(
                                                        uart0,
                                                        "\r\n⚡⚡⚡ [ESP32 ĐÃ ĐƯỢC ĐÁNH THỨC!] ⚡⚡⚡\r\n\
                                                           - Nguồn đánh thức : Cảm biến chạm GPIO4\r\n\
                                                           - Điện dung chuẩn : {}\r\n\
                                                           - Điện dung chạm  : {}\r\n\
                                                           - Độ thay đổi     : Giảm {} đơn vị\r\n\
                                                           - Phản hồi vật lý : Chớp nháy LED GPIO2 chào mừng!\r\n",
                                                        baseline, cur, delta
                                                    );
                                                    // Nhấp nháy LED 3 lần chào mừng
                                                    for _ in 0..3 {
                                                        led.set_high();
                                                        let t = Instant::now();
                                                        while t.elapsed() < Duration::from_millis(100) {}
                                                        led.set_low();
                                                        let t = Instant::now();
                                                        while t.elapsed() < Duration::from_millis(100) {}
                                                    }
                                                } else if waiting_touch && cur >= 320 {
                                                    waiting_touch = false;
                                                    let _ = write!(
                                                        uart0,
                                                        "🍃 [TOUCH RELEASE] Buông tay khỏi GPIO4. Điện dung hồi phục: {}. Trở lại trạng thái chờ...\r\n",
                                                        cur
                                                    );
                                                }

                                                let t = Instant::now();
                                                while t.elapsed() < Duration::from_millis(50) {}
                                            }
                                        }
                                        _ => {
                                            let current = touch_pad.read();
                                            let _ = write!(
                                                uart0,
                                                "-> Cảm biến chạm GPIO4 (Capacitive Touch):\r\n\
                                                   Điện dung hiện tại : {}\r\n\
                                                   Giám sát tự động   : {}\r\n\r\n\
                                                   Các tùy chọn sử dụng:\r\n\
                                                   • touch on    : Kích hoạt giám sát nền (tự thông báo khi chạm)\r\n\
                                                   • touch off   : Tắt giám sát nền\r\n\
                                                   • touch watch : Chế độ chờ ngủ, tự đánh thức ESP32 khi chạm\r\n",
                                                current,
                                                if touch_monitor_active { "ĐANG BẬT" } else { "ĐANG TẮT" }
                                            );
                                        }
                                    }
                                }

                                // ═══ Bài 3+: CPU Benchmark ═══
                                c if c.eq_ignore_ascii_case("benchmark") => {
                                    let limit = arg1.and_then(parse_number).unwrap_or(100_000) as u32;
                                    let _ = write!(uart0, "-> Benchmark: đếm số nguyên tố tới {}...\r\n", limit);
                                    let start = Instant::now();
                                    let primes = count_primes(limit);
                                    let elapsed = start.elapsed();
                                    let _ = write!(
                                        uart0,
                                        "   Kết quả: {} số nguyên tố\r\n   Thời gian: {} ms ({} us)\r\n",
                                        primes,
                                        elapsed.as_millis(),
                                        elapsed.as_micros()
                                    );
                                }

                                // ═══ Bài 5+: Sinh số ngẫu nhiên ═══
                                c if c.eq_ignore_ascii_case("rand") => {
                                    let count = arg1.and_then(parse_number).unwrap_or(1).min(10);
                                    let _ = write!(uart0, "-> Số ngẫu nhiên phần cứng:\r\n");
                                    for i in 1..=count {
                                        let val = rng.random();
                                        let _ = write!(uart0, "   #{}: {}\r\n", i, val);
                                    }
                                }

                                // ═══ Bài 6: Wi-Fi AP + Web Server ═══
                                c if c.eq_ignore_ascii_case("wifi") => {
                                    match arg1 {
                                        Some(s) if s.eq_ignore_ascii_case("off") => {
                                            if wifi_active {
                                                unsafe { esp_wifi_sys_esp32::include::esp_wifi_stop(); }
                                                wifi_active = false;
                                                SYS_WIFI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
                                                let _ = write!(uart0, "-> ĐÃ TẮT sóng Wi-Fi SoftAP.\r\n");
                                            } else {
                                                let _ = write!(uart0, "-> Wi-Fi hiện đang tắt sẵn.\r\n");
                                            }
                                        }
                                        _ => {
                                            if !wifi_started {
                                                let _ = write!(uart0, "-> Khởi động Wi-Fi SoftAP...\r\n");

                                                let ap_config = WifiConfig::AccessPoint(
                                                    AccessPointConfig::default()
                                                        .with_ssid("ESP32-Rust-WiFi".try_into().unwrap())
                                                        .with_authentication(
                                                            esp_radio::wifi::AuthenticationMethodConfig::Wpa2Personal(
                                                                "12345678".try_into().unwrap(),
                                                            ),
                                                        ),
                                                );
                                                let controller_config =
                                                    ControllerConfig::default().with_initial_config(ap_config);
                                                let wifi_peri = wifi_peripheral.take()
                                                    .expect("Wi-Fi peripheral already used");
                                                let mut wifi_controller =
                                                    WifiController::new(wifi_peri, controller_config)
                                                        .expect("Wi-Fi init failed");
                                                let _ = wifi_controller.set_max_tx_power(60);

                                                let wifi_interface = Interface::access_point();

                                                let net_config = Config::ipv4_static(StaticConfigV4 {
                                                    address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 4, 1), 24),
                                                    gateway: Some(Ipv4Address::new(192, 168, 4, 1)),
                                                    dns_servers: Default::default(),
                                                });

                                                static STACK_RESOURCES: StaticCell<StackResources<8>> =
                                                    StaticCell::new();
                                                let resources = STACK_RESOURCES.init(StackResources::new());
                                                let (stack, runner) =
                                                    embassy_net::new(wifi_interface, net_config, resources, 12345);

                                                spawner.spawn(net_task(runner).unwrap());
                                                spawner.spawn(dhcp_task(stack).unwrap());
                                                spawner.spawn(dns_task(stack).unwrap());
                                                spawner.spawn(web_server_task(stack).unwrap());

                                                core::mem::forget(wifi_controller);

                                                wifi_started = true;
                                                wifi_active = true;
                                                SYS_WIFI_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);

                                                let _ = write!(
                                                    uart0,
                                                    "==================================================\r\n\
                                                     Wi-Fi SoftAP đã khởi động thành công!\r\n\
                                                     SSID: ESP32-Rust-WiFi\r\n\
                                                     Mật khẩu: 12345678\r\n\
                                                     Địa chỉ Web: http://192.168.4.1\r\n\
                                                    ==================================================\r\n"
                                                );
                                            } else if !wifi_active {
                                                unsafe { esp_wifi_sys_esp32::include::esp_wifi_start(); }
                                                wifi_active = true;
                                                SYS_WIFI_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);
                                                let _ = write!(uart0, "-> ĐÃ BẬT LẠI Wi-Fi SoftAP! (http://192.168.4.1)\r\n");
                                            } else {
                                                let _ = write!(uart0, "-> Wi-Fi đang chạy (http://192.168.4.1). Gõ 'wifi off' để tắt sóng.\r\n");
                                            }
                                        }
                                    }
                                }

                                // ═══ Bài 8: Deep Sleep ═══
                                c if c.eq_ignore_ascii_case("sleep") => {
                                    let _ = write!(uart0, "-> Chuẩn bị Deep Sleep...\r\n");

                                    // Hẹn giờ nếu có tham số (chưa hỗ trợ timer wakeup, chỉ ghi log)
                                    if let Some(sec_str) = arg1 {
                                        if let Some(_sec) = parse_number(sec_str) {
                                            let _ = write!(uart0, "   (Tham số giây chỉ mang tính tham khảo — đánh thức bằng BOOT)\r\n");
                                        }
                                    }

                                    // Tắt LED (tắt cả onboard GPIO2 và GPIO15)
                                    led.set_low();

                                    // Cấu hình GPIO0 wakeup (dùng lại button đã có)
                                    button.listen(Event::LowLevel);
                                    let wakeup_cfg =
                                        gpio::WakeupConfig::default().with_low_power_path(true);
                                    button
                                        .apply_wakeup_config(&wakeup_cfg)
                                        .expect("Wakeup config failed");

                                    let _ = write!(uart0, "   Nhấn BOOT để đánh thức!\r\n");

                                    // Flush
                                    let t = Instant::now();
                                    while t.elapsed() < Duration::from_millis(100) {}

                                    let sleep_config = RtcSleepConfig::deep();
                                    let mut lp = LowPower::new(peripherals.LPWR);
                                    lp.sleep_deep(sleep_config);
                                    // Không bao giờ tới đây
                                }

                                // ═══ Trạng thái hệ thống ═══
                                c if c.eq_ignore_ascii_case("status") => {
                                    let uptime = boot_time.elapsed().as_secs();
                                    let _ = write!(uart0, "════════════ SYSTEM STATUS ════════════\r\n");
                                    let _ = write!(uart0, " Uptime: {}s\r\n", uptime);
                                    let _ = write!(uart0, " Reset: {:?}\r\n", reset_reason(Cpu::ProCpu));
                                    let _ = write!(uart0, " Wi-Fi: {}\r\n", if wifi_active { "ON (192.168.4.1)" } else { "OFF" });

                                    match led_state {
                                        LedState::Off => {
                                            let _ = write!(uart0, " LED: OFF\r\n");
                                        }
                                        LedState::On { expire_at } => {
                                            if let Some(exp) = expire_at {
                                                let rem = if exp > now { (exp - now).as_secs() } else { 0 };
                                                let _ = write!(uart0, " LED: ON (còn {}s)\r\n", rem);
                                            } else {
                                                let _ = write!(uart0, " LED: ON (vĩnh viễn)\r\n");
                                            }
                                        }
                                        LedState::Blinking { interval, expire_at, .. } => {
                                            let ms = interval.as_millis();
                                            if let Some(exp) = expire_at {
                                                let rem = if exp > now { (exp - now).as_secs() } else { 0 };
                                                let _ = write!(uart0, " LED: BLINK ({}ms, còn {}s)\r\n", ms, rem);
                                            } else {
                                                let _ = write!(uart0, " LED: BLINK ({}ms, vĩnh viễn)\r\n", ms);
                                            }
                                        }
                                    }

                                    let touch_val = touch_pad.read();
                                    let _ = write!(
                                        uart0,
                                        " Touch(GPIO4): {} (Giám sát tự động: {})\r\n",
                                        touch_val,
                                        if touch_monitor_active { "ON" } else { "OFF" }
                                    );
                                    let _ = write!(uart0, "═══════════════════════════════════════\r\n");
                                }

                                // ═══ Trợ giúp ═══
                                c if c.eq_ignore_ascii_case("help") => {
                                    let _ = write!(
                                        uart0,
                                        "╔═══════════════════════════════════════════════════╗\r\n\
                                         ║         DANH SÁCH LỆNH                            ║\r\n\
                                         ╠═══════════════════════════════════════════════════╣\r\n\
                                         ║ on [sec]             Bật LED                      ║\r\n\
                                         ║ off                  Tắt LED                      ║\r\n\
                                         ║ blink [sec] [ms]     Nhấp nháy LED                ║\r\n\
                                         ║ toggle [n]           Đảo LED n lần                ║\r\n\
                                         ║ breathe              LED thở PWM (GPIO2 & GPIO15) ║\r\n\
                                         ║ touch [on|off|watch] Cảm biến chạm tự đánh thức   ║\r\n\
                                         ║ benchmark [n]        CPU benchmark                ║\r\n\
                                         ║ rand [n]             Sinh số ngẫu nhiên           ║\r\n\
                                         ║ wifi [on|off]        Bật/Tắt Wi-Fi AP + Web Server║\r\n\
                                         ║ sleep                Deep Sleep (BOOT wake)       ║\r\n\
                                         ║ status               Trạng thái hệ thống          ║\r\n\
                                         ║ help                 Hiển thị bảng này            ║\r\n\
                                         ╚═══════════════════════════════════════════════════╝\r\n"
                                    );
                                }

                                unknown => {
                                    let _ = write!(uart0, "-> Lệnh không hợp lệ: '{}'. Gõ 'help'.\r\n", unknown);
                                }
                            }
                        }
                        idx = 0;
                    }
                    let _ = write!(uart0, "> ");
                } else if ch == 8 || ch == 127 {
                    // Backspace
                    if idx > 0 {
                        idx -= 1;
                        let _ = write!(uart0, "\x08 \x08");
                    }
                } else if (32..=126).contains(&ch) && idx < buf.len() {
                    // Echo và lưu các ký tự ASCII hiển thị được
                    let _ = write!(uart0, "{}", ch as char);
                    buf[idx] = ch;
                    idx += 1;
                }
            }
        }

        // Yield cho Embassy scheduler
        Timer::after(EmbassyDuration::from_millis(10)).await;
    }
}