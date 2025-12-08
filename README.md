# Infoscreen

This repository contains two display applications:

- Combined lecture plan of a faculty based on JSONRPC-API from [Webuntis](https://webuntis.com)
- Bus stop departures from and into the city based on a JSON API from [DEFAS](https://www.bayerninfo.de/datenangebot/daten-oev)

# Compile with correct windowing support

`cargo build --workspace -F <x11, wayland>` or if you're lazy `--all-features`.
