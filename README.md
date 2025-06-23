# libgtr

This crate provides communication with the PhotonFirst GTR-1001 fiber optic sensing system interrogator ("Gator") over a serial port.
It handles packet parsing, synchronization, and exposes a thread-safe API for receiving parsed data.

## *nix Notes

The FTDI driver is statically compiled into the library, so you do not need to install any additional drivers on Linux.

To access the FTDI USB device as a regular user on Linux you need to update the udev rules.

Create a file called `/etc/udev/rules.d/99-ftdi.rules` with:

```rules
SUBSYSTEM=="usb", ATTRS{idVendor}=="0403", ATTRS{idProduct}=="6001", MODE="0666"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0403", ATTRS{idProduct}=="6010", MODE="0666"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0403", ATTRS{idProduct}=="6011", MODE="0666"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0403", ATTRS{idProduct}=="6014", MODE="0666"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0403", ATTRS{idProduct}=="6015", MODE="0666"
```

Then, reload the rules:

```bash
sudo udevadm control --reload-rules
sudo udevadm trigger
```

If you get an error `DEVICE_NOT_OPENED`, you may need to blacklist the `ftdi_sio` kernel module:

```bash
sudo rmmod ftdi_sio # temporary
echo "blacklist ftdi_sio" | sudo tee /etc/modprobe.d/ftdi_sio.conf # permanent
```
