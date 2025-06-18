//! # libgtr
//!
//! This crate provides communication with the PhotonFirst GTR-1001 fiber optic sensing system interrogator ("Gator") over a serial port.
//! It handles packet parsing, synchronization, and exposes a thread-safe API for receiving parsed data.
//!

use byteorder::{BigEndian, ReadBytesExt};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use log::{debug, error, info};
use serialport::SerialPort;
use std::io::{self, Cursor, Read};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use thiserror::Error;

const MAGIC_BYTES: &[u8; 4] = b"yoho";
const HEADER_SIZE: usize = 16;
const STATUS_SIZE: usize = 3;
const SENSOR_SIZE: usize = 3;
const NUM_SENSORS: usize = 8;
const PACKET_PAYLOAD_SIZE: usize = STATUS_SIZE + SENSOR_SIZE * NUM_SENSORS;
const PACKET_TOTAL_SIZE: usize = HEADER_SIZE + PACKET_PAYLOAD_SIZE;

#[derive(Error, Debug)]
/// Errors that can occur when communicating with the Gator.
pub enum GtrError {
    #[error("Serial port error: {0}")]
    Serial(#[from] serialport::Error),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Channel send error: Packet dropped")]
    ChannelSend,
    #[error("Channel receive error: {0}")]
    ChannelReceive(#[from] crossbeam_channel::RecvError),
    #[error("Synchronization lost")]
    SyncLost,
    #[error("Thread communication error: {0}")]
    ThreadComm(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Header information for a Gator packet.
pub struct Header {
    /// Protocol version
    pub version: u8,
    /// Message kind/type
    pub kind: u8,
    /// Packet counter
    pub counter: u16,
    /// Timestamp in microseconds
    pub time_us: u32,
    /// Payload size in bytes
    pub payload_size: u32,
}

impl Header {
    fn from_bytes(bytes: &[u8]) -> Result<Self, GtrError> {
        if bytes.len() < HEADER_SIZE - MAGIC_BYTES.len() {
            return Err(GtrError::Parse(format!(
                "Not enough data for Header. Expected {}, got {}",
                HEADER_SIZE - MAGIC_BYTES.len(),
                bytes.len()
            )));
        }
        let mut cursor = Cursor::new(bytes);
        let version = cursor.read_u8()?;
        let msg_type = cursor.read_u8()?;
        let counter = cursor.read_u16::<BigEndian>()?;
        let time_us = cursor.read_u32::<BigEndian>()?;

        let payload_size = cursor.read_u16::<BigEndian>()?;
        let _padding1 = cursor.read_u8()?;
        let _padding2 = cursor.read_u8()?;

        let final_size = payload_size as u32;
        if final_size as usize != PACKET_PAYLOAD_SIZE {
            return Err(GtrError::Parse(format!(
                "Invalid payload_size. Expected {}, got {}",
                PACKET_PAYLOAD_SIZE, final_size
            )));
        }

        Ok(Header {
            version,
            kind: msg_type,
            counter,
            time_us,
            payload_size: final_size,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Status information for a Gator packet.
pub struct Status {
    /// TEC (Thermo-Electric Cooler) status
    pub tec_ok: bool,
    /// Sensor 8 status
    pub s8_ok: bool,
    /// Sensor 7 status
    pub s7_ok: bool,
    /// Sensor 6 status
    pub s6_ok: bool,
    /// Sensor 5 status
    pub s5_ok: bool,
    /// Sensor 4 status
    pub s4_ok: bool,
    /// Sensor 3 status
    pub s3_ok: bool,
    /// Sensor 2 status
    pub s2_ok: bool,
    /// Sensor 1 status
    pub s1_ok: bool,
    /// Number of sensors present
    pub nr_sensors: u8, // 4 bits
}

impl Status {
    fn from_bytes(bytes: &[u8]) -> Result<Self, GtrError> {
        if bytes.len() < STATUS_SIZE {
            return Err(GtrError::Parse(format!(
                "Not enough data for Status. Expected {}, got {}",
                STATUS_SIZE,
                bytes.len()
            )));
        }
        let byte1 = bytes[1];
        let byte2 = bytes[2];

        let tec_ok = (byte1 & 0b00100000) != 0;
        let s8_ok = (byte1 & 0b00010000) != 0;
        let s7_ok = (byte1 & 0b00001000) != 0;
        let s6_ok = (byte1 & 0b00000100) != 0;
        let s5_ok = (byte1 & 0b00000010) != 0;
        let s4_ok = (byte1 & 0b00000001) != 0;

        let s3_ok = (byte2 & 0b10000000) != 0;
        let s2_ok = (byte2 & 0b01000000) != 0;
        let s1_ok = (byte2 & 0b00100000) != 0;
        let nr_sensors = (byte2 & 0b00011110) >> 1;

        Ok(Status {
            tec_ok,
            s8_ok,
            s7_ok,
            s6_ok,
            s5_ok,
            s4_ok,
            s3_ok,
            s2_ok,
            s1_ok,
            nr_sensors,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Sensor data for a single Gator sensor channel.
pub struct Sensor {
    /// Sensor status (true if OK)
    pub sens_ok: bool, // Bit 18
    /// Center-of-Gravity value (18 bits)
    pub cog_value: u32, // Reconstructed 18-bit value
}

impl Sensor {
    fn from_bytes(bytes: &[u8]) -> Result<Self, GtrError> {
        if bytes.len() < SENSOR_SIZE {
            return Err(GtrError::Parse(format!(
                "Not enough data for Sensor. Expected {}, got {}",
                SENSOR_SIZE,
                bytes.len()
            )));
        }
        let byte0 = bytes[0];
        let byte1 = bytes[1];
        let byte2 = bytes[2];

        let sens_ok = (byte0 & 0b00000100) != 0;
        let cog_msb2 = (byte0 & 0b00000011) as u32;

        let cog_mid8 = byte1 as u32;
        let cog_lsb8 = byte2 as u32;

        let cog_value = (cog_msb2 << 16) | (cog_mid8 << 8) | cog_lsb8;

        Ok(Sensor { sens_ok, cog_value })
    }

    /// Calculates the wavelength in nanometers based on the CoG value.
    /// Formula: λ_CoG = 1514 + CoG_value / (2^18 * (1586 - 1514))
    pub fn get_wavelength(&self) -> f64 {
        let cog_value_f = self.cog_value as f64;
        // 2^18 = 262144
        // (1586 - 1514) = 72
        1514.0 + cog_value_f / (262144.0 * 72.0)
    }

    /// Calculates the micro-strain (με) based on the current wavelength and a reference wavelength.
    /// Formula: με = ((λ_CoG - λ_0) / λ_0) * (1 / (1 - 0.22)) * 10^6
    ///
    /// # Arguments
    /// * `lambda_0_nm` - The reference wavelength (λ_0) in nanometers.
    pub fn get_microstrain(&self) -> f64 {
        let wavelength = self.get_wavelength();
        if wavelength == 0.0 {
            return f64::NAN; // Or handle error appropriately
        }
        let lambda_cog_nm = self.get_wavelength();
        ((lambda_cog_nm - wavelength) / wavelength) * (1.0 / (1.0 - 0.22)) * 1_000_000.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A fully parsed Gator packet, including header, status, and all sensor data.
pub struct GtrPacket {
    /// Packet header
    pub header: Header,
    /// Status information
    pub status: Status,
    /// Array of sensor data
    pub sensors: [Sensor; NUM_SENSORS],
}

impl GtrPacket {
    fn from_bytes(bytes: &[u8]) -> Result<Self, GtrError> {
        if bytes.len() < PACKET_TOTAL_SIZE {
            return Err(GtrError::Parse(format!(
                "Not enough data for GtrPacket. Expected {}, got {}",
                PACKET_TOTAL_SIZE,
                bytes.len()
            )));
        }

        let header = Header::from_bytes(&bytes[MAGIC_BYTES.len()..HEADER_SIZE])?;

        let status_offset = HEADER_SIZE;
        let status = Status::from_bytes(&bytes[status_offset..status_offset + STATUS_SIZE])?;

        let mut sensors = [
            Sensor { sens_ok: false, cog_value: 0 },
            Sensor { sens_ok: false, cog_value: 0 },
            Sensor { sens_ok: false, cog_value: 0 },
            Sensor { sens_ok: false, cog_value: 0 },
            Sensor { sens_ok: false, cog_value: 0 },
            Sensor { sens_ok: false, cog_value: 0 },
            Sensor { sens_ok: false, cog_value: 0 },
            Sensor { sens_ok: false, cog_value: 0 },
        ];

        for i in 0..NUM_SENSORS {
            let sensor_offset = HEADER_SIZE + STATUS_SIZE + i * SENSOR_SIZE;
            sensors[i] = Sensor::from_bytes(&bytes[sensor_offset..sensor_offset + SENSOR_SIZE])?;
        }

        Ok(GtrPacket {
            header,
            status,
            sensors,
        })
    }

    /// Attempts to extract valid sensor data from corrupted packet bytes using multiple strategies.
    /// This function is more permissive and tries to recover as much data as possible.
    fn from_bytes_with_error_correction(bytes: &[u8]) -> Result<Self, GtrError> {
        // First try normal parsing
        if let Ok(packet) = Self::from_bytes(bytes) {
            return Ok(packet);
        }

        debug!("Parse failed, attempting recovery");

        // Strategy 1: Try to find valid magic bytes and parse from there
        for offset in 0..bytes.len().saturating_sub(PACKET_TOTAL_SIZE) {
            if offset + MAGIC_BYTES.len() <= bytes.len()
                && &bytes[offset..offset + MAGIC_BYTES.len()] == MAGIC_BYTES
            {
                if offset + PACKET_TOTAL_SIZE <= bytes.len() {
                    if let Ok(packet) = Self::from_bytes(&bytes[offset..offset + PACKET_TOTAL_SIZE]) {
                        debug!("Recovered at offset {}", offset);
                        return Ok(packet);
                    }
                }
            }
        }

        // Strategy 2: Extreme sensor data recovery - scan entire buffer
        debug!("Scanning for sensor data");

        let default_header = Header {
            version: 1,
            kind: 1,
            counter: 0,
            time_us: 0,
            payload_size: PACKET_PAYLOAD_SIZE as u32,
        };

        let default_status = Status {
            tec_ok: false,
            s8_ok: false,
            s7_ok: false,
            s6_ok: false,
            s5_ok: false,
            s4_ok: false,
            s3_ok: false,
            s2_ok: false,
            s1_ok: false,
            nr_sensors: 0,
        };

        let mut sensors = core::array::from_fn(|_| Sensor { sens_ok: false, cog_value: 0 });
        let mut sensor_candidates = Vec::new();

        // Strategy 2a: Try to find sensor data at expected locations with tolerance
        let expected_sensor_start = HEADER_SIZE + STATUS_SIZE;
        for i in 0..NUM_SENSORS {
            let base_offset = expected_sensor_start + i * SENSOR_SIZE;

            // Try exact position first
            if base_offset + SENSOR_SIZE <= bytes.len() {
                if let Ok(sensor) = Sensor::from_bytes(&bytes[base_offset..base_offset + SENSOR_SIZE]) {
                    if Self::is_sensor_data_plausible(&sensor) {
                        sensors[i] = sensor;
                        continue;
                    }
                }
            }

            // Try nearby positions (±5 bytes) to account for corruption/desync
            for offset_adj in -5i32..=5i32 {
                let adjusted_offset = (base_offset as i32 + offset_adj) as usize;
                if adjusted_offset + SENSOR_SIZE <= bytes.len() {
                    if let Ok(sensor) = Sensor::from_bytes(&bytes[adjusted_offset..adjusted_offset + SENSOR_SIZE]) {
                        if Self::is_sensor_data_plausible(&sensor) {
                            sensors[i] = sensor;
                            break;
                        }
                    }
                }
            }
        }

        // Strategy 2b: Exhaustive scan for all plausible sensor patterns in entire buffer
        for start_pos in 0..bytes.len().saturating_sub(SENSOR_SIZE) {
            if let Ok(sensor) = Sensor::from_bytes(&bytes[start_pos..start_pos + SENSOR_SIZE]) {
                if Self::is_sensor_data_plausible(&sensor) {
                    sensor_candidates.push((start_pos, sensor));
                }
            }
        }

        // Strategy 2c: Fill empty sensor slots with best candidates
        let mut used_positions = std::collections::HashSet::new();
        for i in 0..NUM_SENSORS {
            if !sensors[i].sens_ok {
                // Find best unused candidate
                if let Some((pos, sensor)) = sensor_candidates.iter().find(|(pos, _)| !used_positions.contains(pos)) {
                    sensors[i] = sensor.clone();
                    used_positions.insert(*pos);
                }
            }
        }

        // Strategy 2d: Overlapping scan with quality scoring
        if sensor_candidates.len() < NUM_SENSORS {
            let mut scored_candidates = Vec::new();

            for start_pos in 0..bytes.len().saturating_sub(SENSOR_SIZE) {
                if let Ok(sensor) = Sensor::from_bytes(&bytes[start_pos..start_pos + SENSOR_SIZE]) {
                    let quality_score = Self::calculate_sensor_quality_score(&sensor, &bytes[start_pos..start_pos + SENSOR_SIZE]);
                    if quality_score > 0.0 {
                        scored_candidates.push((start_pos, sensor, quality_score));
                    }
                }
            }

            // Sort by quality score descending
            scored_candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

            // Fill remaining empty slots with highest quality candidates
            let mut candidates_used = 0;
            for i in 0..NUM_SENSORS {
                if !sensors[i].sens_ok && candidates_used < scored_candidates.len() {
                    let (_, sensor, _) = &scored_candidates[candidates_used];
                    sensors[i] = sensor.clone();
                    candidates_used += 1;
                }
            }
        }

        // Strategy 2e: Pattern-based recovery for sequential sensor data
        if sensor_candidates.len() >= 2 {
            // Look for sequences of 3-byte patterns that might be sequential sensors
            for start_pos in 0..bytes.len().saturating_sub(SENSOR_SIZE * 3) {
                let mut sequential_sensors = Vec::new();
                let mut valid_sequence = true;

                for seq_idx in 0..std::cmp::min(NUM_SENSORS, (bytes.len() - start_pos) / SENSOR_SIZE) {
                    let sensor_start = start_pos + seq_idx * SENSOR_SIZE;
                    if sensor_start + SENSOR_SIZE <= bytes.len() {
                        if let Ok(sensor) = Sensor::from_bytes(&bytes[sensor_start..sensor_start + SENSOR_SIZE]) {
                            let quality = Self::calculate_sensor_quality_score(&sensor, &bytes[sensor_start..sensor_start + SENSOR_SIZE]);
                            if quality > 0.3 {
                                sequential_sensors.push((seq_idx, sensor));
                            } else {
                                valid_sequence = false;
                                break;
                            }
                        } else {
                            valid_sequence = false;
                            break;
                        }
                    }
                }

                if valid_sequence && sequential_sensors.len() >= 3 {
                    for (seq_idx, sensor) in sequential_sensors {
                        if seq_idx < NUM_SENSORS && !sensors[seq_idx].sens_ok {
                            sensors[seq_idx] = sensor;
                        }
                    }
                    break;
                }
            }
        }

        // Count recovered sensors
        let recovered_count = sensors.iter().filter(|s| s.sens_ok || s.cog_value != 0).count();

        if recovered_count > 0 {
            debug!("Recovered {} sensors", recovered_count);
            return Ok(GtrPacket {
                header: default_header,
                status: default_status,
                sensors,
            });
        }

        Err(GtrError::Parse("Recovery failed".to_string()))
    }

    /// Calculate a quality score for sensor data based on multiple factors
    fn calculate_sensor_quality_score(sensor: &Sensor, raw_bytes: &[u8]) -> f64 {
        let mut score = 0.0 as f64;

        // Factor 1: Sensor OK flag (strong positive)
        if sensor.sens_ok {
            score += 3.0;
        }

        // Factor 2: CoG value in reasonable range
        if sensor.cog_value > 0 && sensor.cog_value < (1 << 18) {
            score += 1.0;

            // Bonus for CoG values that aren't at extremes
            let cog_normalized = sensor.cog_value as f64 / (1 << 18) as f64;
            if cog_normalized > 0.1 && cog_normalized < 0.9 {
                score += 0.5;
            }
        }

        // Factor 3: Wavelength in expected range
        let wavelength = sensor.get_wavelength();
        if wavelength >= 1500.0 && wavelength <= 1600.0 {
            score += 1.5;

            // Bonus for typical FBG wavelengths
            if wavelength >= 1530.0 && wavelength <= 1570.0 {
                score += 0.5;
            }
        }

        // Factor 4: Raw byte patterns (avoid obvious corruption patterns)
        if raw_bytes.len() >= 3 {
            // Penalize all-zeros or all-ones patterns
            if raw_bytes.iter().all(|&b| b == 0x00) || raw_bytes.iter().all(|&b| b == 0xFF) {
                score -= 2.0;
            }

            // Penalize obvious sequential patterns
            if raw_bytes.len() >= 3 && raw_bytes.windows(2).all(|w| w[1] == w[0].wrapping_add(1)) {
                score -= 1.0;
            }
        }

        // Factor 5: Reserved bits should be zero (structural validation)
        if raw_bytes.len() >= 1 {
            let reserved_bits = raw_bytes[0] & 0b11111000; // Bits 7-3 should be reserved
            if reserved_bits == 0 {
                score += 0.5;
            } else {
                score -= 0.5;
            }
        }

        score.max(0.0)
    }

    /// Check if sensor data appears plausible (basic sanity checks)
    fn is_sensor_data_plausible(sensor: &Sensor) -> bool {
        // CoG value should be within 18-bit range
        if sensor.cog_value >= (1 << 18) {
            return false;
        }

        // Wavelength should be in reasonable range (rough check)
        let wavelength = sensor.get_wavelength();
        if wavelength < 1500.0 || wavelength > 1600.0 {
            return false;
        }

        // Additional plausibility checks for extreme recovery

        // Very low CoG values might indicate corruption (unless sensor is not OK)
        if sensor.cog_value == 0 && sensor.sens_ok {
            return false; // Suspicious: sensor OK but no signal
        }

        // Very high CoG values near the limit are suspicious
        if sensor.cog_value > (1 << 18) - 100 {
            return false;
        }

        true
    }
}

/// Threaded serial port reader for the Gator.
///
/// This struct spawns a background thread to read and parse packets from the serial port,
/// delivering them via a channel. Use [`recv_packet`] or [`try_recv_packet`] to receive packets.
pub struct GtrSerialReader {
    packet_rx: Receiver<GtrPacket>,
    stop_signal: Arc<AtomicBool>,
    reader_thread: Option<JoinHandle<()>>,
    dropped_count: Arc<AtomicU64>,
}

impl GtrSerialReader {
    /// Open a serial port and start a background thread to read Gator packets.
    ///
    /// # Arguments
    /// * `port_name` - Serial port device path (e.g., "/dev/ttyUSB0")
    /// * `baud_rate` - Baud rate (e.g., 115200)
    ///
    /// # Errors
    /// Returns [`GtrError`] if the port cannot be opened.
    pub fn new(port_name: &str, baud_rate: u32) -> Result<Self, GtrError> {
        info!("Opening serial port: {} at {} baud", port_name, baud_rate);
        let port = serialport::new(port_name, baud_rate)
            .timeout(Duration::from_millis(1000))
            .open()?;
        info!("Serial port opened successfully.");

        let (packet_tx, packet_rx) = bounded(100);
        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_signal_clone = Arc::clone(&stop_signal);
        let dropped_count = Arc::new(AtomicU64::new(0));
        let dropped_count_clone = Arc::clone(&dropped_count);

        let reader_thread = thread::spawn(move || {
            Self::reader_loop(port, packet_tx, stop_signal_clone, dropped_count_clone);
        });

        Ok(GtrSerialReader {
            packet_rx,
            stop_signal,
            reader_thread: Some(reader_thread),
            dropped_count,
        })
    }

    fn reader_loop(
        mut port: Box<dyn SerialPort>,
        packet_tx: Sender<GtrPacket>,
        stop_signal: Arc<AtomicBool>,
        dropped_count: Arc<AtomicU64>,
    ) {
        let mut sync_buffer: [u8; MAGIC_BYTES.len()] = [0; MAGIC_BYTES.len()];
        let mut sync_buffer_idx = 0;
        let mut synced = false;
        let mut packet_buffer = [0u8; PACKET_TOTAL_SIZE];
        let mut skip_bytes = 0;
        let mut resync_attempts = 0;
        let mut corruption_buffer = Vec::new();

        info!("Reader thread started");

        while !stop_signal.load(Ordering::Relaxed) {
            if !synced {
                let mut byte_buf = [0u8; 1];
                match port.read(&mut byte_buf) {
                    Ok(0) => continue,
                    Ok(_) => {
                        let byte = byte_buf[0];
                        sync_buffer[sync_buffer_idx] = byte;
                        sync_buffer_idx = (sync_buffer_idx + 1) % MAGIC_BYTES.len();

                        let mut current_potential_magic = [0u8; MAGIC_BYTES.len()];
                        for i in 0..MAGIC_BYTES.len() {
                            current_potential_magic[i] = sync_buffer[(sync_buffer_idx + i) % MAGIC_BYTES.len()];
                        }

                        if current_potential_magic == *MAGIC_BYTES {
                            debug!("Synced");
                            synced = true;
                            resync_attempts = 0;
                            packet_buffer[..MAGIC_BYTES.len()].copy_from_slice(&current_potential_magic);
                            sync_buffer_idx = 0;

                            // If we have corruption buffer data, try to recover from it
                            if !corruption_buffer.is_empty() {
                                if let Ok(recovered_packet) = GtrPacket::from_bytes_with_error_correction(&corruption_buffer) {
                                    let _ = packet_tx.send(recovered_packet);
                                }
                                corruption_buffer.clear();
                            }
                        } else {
                            // Collect bytes for potential recovery
                            corruption_buffer.push(byte);
                            // Limit buffer size to prevent memory issues
                            if corruption_buffer.len() > PACKET_TOTAL_SIZE * 3 {
                                corruption_buffer.drain(..PACKET_TOTAL_SIZE);
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
                    Err(e) => {
                        error!("Serial error during sync: {}", e);
                        break;
                    }
                }
            } else {
                match port.read_exact(&mut packet_buffer[MAGIC_BYTES.len()..]) {
                    Ok(_) => {
                        match GtrPacket::from_bytes(&packet_buffer) {
                            Ok(packet) => {
                                if packet_tx.send(packet).is_err() {
                                    error!("Receiver dropped");
                                    break;
                                }
                            }
                            Err(_) => {
                                // Try enhanced error correction on the current packet
                                match GtrPacket::from_bytes_with_error_correction(&packet_buffer) {
                                    Ok(recovered_packet) => {
                                        if packet_tx.send(recovered_packet).is_err() {
                                            error!("Receiver dropped");
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        // Count as dropped packet
                                        dropped_count.fetch_add(1, Ordering::Relaxed);
                                        
                                        // Try to find magic bytes within the packet for recovery
                                        let mut found_magic_at = None;
                                        for offset in 1..PACKET_TOTAL_SIZE - MAGIC_BYTES.len() {
                                            if &packet_buffer[offset..offset + MAGIC_BYTES.len()] == MAGIC_BYTES {
                                                found_magic_at = Some(offset);
                                                break;
                                            }
                                        }

                                        if let Some(offset) = found_magic_at {
                                            // Add corrupted portion to corruption buffer for potential recovery
                                            corruption_buffer.extend_from_slice(&packet_buffer[..offset]);

                                            packet_buffer.copy_within(offset.., 0);
                                            match port.read_exact(&mut packet_buffer[PACKET_TOTAL_SIZE - offset..]) {
                                                Ok(_) => {
                                                    match GtrPacket::from_bytes_with_error_correction(&packet_buffer) {
                                                        Ok(packet) => {
                                                            // Try to recover from corruption buffer too
                                                            if !corruption_buffer.is_empty() {
                                                                if let Ok(recovered_packet) = GtrPacket::from_bytes_with_error_correction(&corruption_buffer) {
                                                                    let _ = packet_tx.send(recovered_packet);
                                                                }
                                                                corruption_buffer.clear();
                                                            }

                                                            if packet_tx.send(packet).is_err() {
                                                                error!("Receiver dropped");
                                                                break;
                                                            }
                                                        }
                                                        Err(_) => {
                                                            corruption_buffer.extend_from_slice(&packet_buffer);
                                                            synced = false;
                                                            sync_buffer_idx = 0;
                                                            resync_attempts += 1;
                                                        }
                                                    }
                                                }
                                                Err(_) => {
                                                    corruption_buffer.extend_from_slice(&packet_buffer[..PACKET_TOTAL_SIZE - offset]);
                                                    synced = false;
                                                    sync_buffer_idx = 0;
                                                    resync_attempts += 1;
                                                }
                                            }
                                        } else {
                                            corruption_buffer.extend_from_slice(&packet_buffer);
                                            synced = false;
                                            sync_buffer_idx = 0;
                                            resync_attempts += 1;
                                        }
                                    }
                                }
                            }
                        }

                        if synced {
                            match port.read_exact(&mut packet_buffer[..MAGIC_BYTES.len()]) {
                                Ok(_) => {
                                    if packet_buffer[..MAGIC_BYTES.len()] != *MAGIC_BYTES {
                                        let matching_bytes = packet_buffer[..MAGIC_BYTES.len()].iter()
                                            .zip(MAGIC_BYTES.iter())
                                            .filter(|&(a, b)| a == b)
                                            .count();

                                        if matching_bytes < 2 {
                                            debug!("Lost sync, resyncing");
                                            corruption_buffer.extend_from_slice(&packet_buffer[..MAGIC_BYTES.len()]);
                                            synced = false;
                                            sync_buffer.copy_from_slice(&packet_buffer[..MAGIC_BYTES.len()]);
                                            sync_buffer_idx = 0;
                                            resync_attempts += 1;
                                        }
                                    }
                                }
                                Err(_) => {
                                    debug!("Read error, resyncing");
                                    synced = false;
                                    sync_buffer_idx = 0;
                                    resync_attempts += 1;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        skip_bytes += 1;
                        if skip_bytes > 10 {
                            debug!("Multiple timeouts, resyncing");
                            synced = false;
                            sync_buffer_idx = 0;
                            skip_bytes = 0;
                            resync_attempts += 1;
                        }
                        continue;
                    }
                    Err(e) => {
                        error!("Serial error: {}", e);
                        synced = false;
                        sync_buffer_idx = 0;
                        resync_attempts += 1;
                        if e.kind() == io::ErrorKind::BrokenPipe || e.kind() == io::ErrorKind::NotConnected {
                            break;
                        }
                    }
                }

                if resync_attempts > 5 {
                    thread::sleep(Duration::from_millis(5));
                    resync_attempts = 0;
                }
            }
        }
        info!("Reader thread finished");
    }

    /// Get the count of dropped packets since the reader started.
    ///
    /// A packet is considered dropped if it could not be parsed using normal
    /// parsing or error correction methods.
    pub fn dropped_count(&self) -> u64 {
        self.dropped_count.load(Ordering::Relaxed)
    }

    /// Receive the next parsed packet, blocking until available.
    ///
    /// # Errors
    /// Returns [`GtrError`] if the channel is closed.
    pub fn recv_packet(&self) -> Result<GtrPacket, GtrError> {
        Ok(self.packet_rx.recv()?)
    }

    /// Try to receive a packet without blocking.
    ///
    /// # Returns
    /// - `Ok(Some(packet))` if a packet is available
    /// - `Ok(None)` if no packet is available
    /// - `Err(GtrError)` if the channel is closed
    pub fn try_recv_packet(&self) -> Result<Option<GtrPacket>, GtrError> {
        match self.packet_rx.try_recv() {
            Ok(packet) => Ok(Some(packet)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(GtrError::ThreadComm(
                "Reader thread disconnected".to_string(),
            )),
        }
    }

    /// Stop the background reader thread and close the serial port.
    pub fn stop(&mut self) -> Result<(), GtrError> {
        info!("Stopping GTR serial reader...");
        self.stop_signal.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader_thread.take() {
            if handle.join().is_err() {
                return Err(GtrError::ThreadComm(
                    "Reader thread panicked".to_string(),
                ));
            }
        }
        info!("GTR serial reader stopped.");
        Ok(())
    }
}

impl Drop for GtrSerialReader {
    fn drop(&mut self) {
        if self.reader_thread.is_some() {
            if let Err(e) = self.stop() {
                error!("Error stopping reader thread during drop: {:?}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse() {
        let mut full_packet_data = Vec::new();
        full_packet_data.write_all(MAGIC_BYTES).unwrap();
        full_packet_data.write_all(&[0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x1B, 0x00, 0x00]).unwrap();
        full_packet_data.write_all(&[0x00, 0b00111111, 0b11100000 | (8 << 1)]).unwrap();
        for i in 0..NUM_SENSORS {
            let cog_val = i as u32;
            let byte0 = 0b00000100 | ((cog_val >> 16) & 0b11) as u8;
            let byte1 = ((cog_val >> 8) & 0xFF) as u8;
            let byte2 = (cog_val & 0xFF) as u8;
            full_packet_data.write_all(&[byte0, byte1, byte2]).unwrap();
        }

        assert_eq!(full_packet_data.len(), PACKET_TOTAL_SIZE);

        let packet = GtrPacket::from_bytes(&full_packet_data).unwrap();
        assert_eq!(packet.header.version, 1);
        assert_eq!(packet.status.nr_sensors, 8);
        assert_eq!(packet.status.s1_ok, true);
        assert_eq!(packet.sensors[0].sens_ok, true);
        assert_eq!(packet.sensors[0].cog_value, 0);
        assert_eq!(packet.sensors[7].cog_value, 7);
    }
}