use chrono::Local;
use csv::Writer;
use libgtr::{GtrPacket, GtrSerialReader};
use std::{
    fs::File,
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn write_csv_record(wtr: &mut Writer<File>, packet: &GtrPacket, count: u64) -> Result<(), csv::Error> {
    let mut record = vec![
        count.to_string(),
        packet.header.time_us.to_string(),
        packet.status.tec_ok.to_string(),
        packet.status.nr_sensors.to_string(),
    ];

    for sensor in &packet.sensors {
        record.push(sensor.sens_ok.to_string());
        record.push(sensor.cog_value.to_string());
        record.push(format!("{:.4}", sensor.get_wavelength()));
    }
    wtr.write_record(&record)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = GtrSerialReader::new()?;

    // --- CSV Setup ---
    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
    let csv_filename = format!("gtr_data_{}.csv", timestamp);
    let mut wtr = Writer::from_path(&csv_filename)?;
    println!("Writing data to {}", csv_filename);

    let mut header = vec!["packet_counter".to_string(), "timestamp_us".to_string(), "tec_ok".to_string(), "nr_sensors".to_string()];
    for i in 1..=8 {
        header.push(format!("s{}_ok", i));
        header.push(format!("s{}_cog", i));
        header.push(format!("s{}_wavelength_nm", i));
    }
    wtr.write_record(&header)?;

    // --- Graceful Shutdown Setup ---
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;
    println!("Press Ctrl+C to stop recording.");

    // --- Status & Loop Setup ---
    let mut packet_count = 0u64;
    let mut last_status_time = Instant::now();
    let mut packets_since_update = 0u64;
    let status_interval = Duration::from_millis(500);

    while running.load(Ordering::SeqCst) {
        match reader.try_recv_packet()? {
            Some(packet) => {
                packet_count += 1;
                packets_since_update += 1;
                write_csv_record(&mut wtr, &packet, packet_count)?;
            }
            None => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // --- Status Update ---
        let now = Instant::now();
        if now.duration_since(last_status_time) >= status_interval {
            let elapsed_secs = now.duration_since(last_status_time).as_secs_f64();
            let rate = packets_since_update as f64 / elapsed_secs;
            let dropped = reader.dropped_count();
            let file_size_mb = wtr.get_ref().metadata()?.len() as f64 / (1024.0 * 1024.0);

            print!(
                "\rStatus | Rate: {:>7.2} Hz | Packets: {:>8} | Dropped: {:>5} | File Size: {:>8.2} MB",
                rate, packet_count, dropped, file_size_mb
            );
            io::stdout().flush()?;

            last_status_time = now;
            packets_since_update = 0;
        }
    }

    // --- Finalization ---
    wtr.flush()?;
    let final_dropped = reader.dropped_count();
    let final_file_size = File::open(&csv_filename)?.metadata()?.len();

    println!("\n\nFinished recording.");
    println!("Wrote {} packets to {}", packet_count, csv_filename);
    println!("Final file size: {:.2} MB", final_file_size as f64 / (1024.0 * 1024.0));
    println!("Total packets dropped: {}", final_dropped);

    reader.stop()?;
    Ok(())
}