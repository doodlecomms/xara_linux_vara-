# Changelog

## 0.1.3

- macOS / CoreAudio support thanks to NR5P's work in x0rro/xara#19
- FreeBSD support thanks to dcasati's work in x0rro/xara#29
- experimental (tr)uSDX support, thanks to VA3SHC's work in x0rro/xara#12
- revamped TX latency model

## 0.1.2

- `--log` produces timestamped log files
- new audio resampling and buffering path
- bugfix: call signs with SSID are encoded properly

## 0.1.1

recording feature and bugfixes

- `--record` flag captures rx, tx, and PTT in FLAC
- bugfix: long squelch tails no longer trigger false receive
- bugfix: phantom frames no longer send inappropriate compression

## 0.1.0

first release

- modem is fully compatible with VARA HF 4.9.0 and VARA FM 4.4.0
- web UI for configuration and management
- all the usual TCP ports (works with your favorite software)
- pipewire, pulse, and alsa support

