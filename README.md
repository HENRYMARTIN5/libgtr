# libgtr

This crate provides communication with the PhotonFirst GTR-1001 fiber optic sensing system interrogator ("Gator") over a serial port.
It handles packet parsing, synchronization, and exposes a thread-safe API for receiving parsed data.
