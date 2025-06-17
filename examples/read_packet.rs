use libgtr::{GtrError, GtrSerialReader};
use std::{env, time::Duration, thread};
use simple_logger;

fn main() -> Result<(), GtrError> {
    simple_logger::SimpleLogger::new().with_level(log::LevelFilter::Error).init().unwrap();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <serial_port_path> [baud_rate]", args[0]);
        eprintln!("Example: {} /dev/ttyUSB0 115200", args[0]);
        return Ok(());
    }

    let port_name = &args[1];
    let baud_rate = if args.len() > 2 {
        args[2].parse::<u32>().unwrap_or(115200)
    } else {
        115200
    };

    let mut reader = GtrSerialReader::new(port_name, baud_rate)?;

    println!("Press Ctrl+C to stop.");
    let start_time = std::time::Instant::now();
    loop {
        match reader.try_recv_packet() {
            Ok(Some(packet)) => {
                println!(
                    "Received Packet: Counter={}, Time={}us, S1_OK={}, S1_CoG={}",
                    packet.header.counter,
                    packet.header.time_us,
                    packet.status.s1_ok,
                    packet.sensors[0].cog_value
                );
            }
            Ok(None) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(GtrError::ThreadComm(msg)) => {
                eprintln!("Reader thread communication error: {}. Exiting.", msg);
                break;
            }
            Err(e) => {
                eprintln!("Unexpected error receiving packet: {:?}", e);
                break;
            }
        }

        if start_time.elapsed() > Duration::from_secs(3000) {
            println!("Timeout reached. Stopping reader.");
            break;
        }
    }

    reader.stop()?;
    println!("Program finished.");
    Ok(())
}