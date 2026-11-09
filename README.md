# mmspad

Use an Android tablet as a pen tablet and touchpad for a Linux PC, over the USB cable.

- **Pen:** pressure, tilt, hover and the pen button, as a real tablet pen.
- **Fingers:** the PC's touchpad: move, tap to click, two fingers to scroll. A button on the tablet turns them on and off.
- **Screen:** optional. The PC's screen on the tablet, encoded on the GPU (VA-API) only while the tablet shows it.

Everything goes through `adb reverse` on the USB cable. The server listens on 127.0.0.1 only; nothing is open to the network.

## Install

On the PC (Arch Linux):

```sh
cd packaging && makepkg -si
systemctl --user enable --now mmspad
```

Your user must be in the `input` group (for `/dev/uinput`). Build the tablet app with
`cd android && ./gradlew assembleDebug` and install it with `adb install`.

On the tablet, turn on USB debugging and allow the PC. When you plug the tablet in, a
udev rule runs `adb reverse` for it, and the app connects on its own.

## Run

- `systemctl --user stop mmspad` and `systemctl --user start mmspad`
- `mmspad --help` for the options (port, frame rate, bitrate).

## Credits

Started as a fork of [drawing_tablet](https://github.com/lemonxah/drawing_tablet) by lemonxah (MIT).
