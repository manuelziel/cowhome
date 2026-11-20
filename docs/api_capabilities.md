# API-Fähigkeiten (FritzBox Smart Home & Core)

In diesem Dokument ist aufgelistet, welche physischen Messwerte und Aktionen wir über unsere eigene API bei den einzelnen Geräten direkt auslesen und steuern.
**Grundprinzip (100 % CowHome-Backend):** Alle intelligenten Konfigurationen – insbesondere **Gerätenamen (`custom_names`)**, **virtuelle Software-Gruppen**, **Komfort- und Absenktemperaturen (`comfort_temperature` / `eco_temperature`)**, **Raumfühler-Abhängigkeiten (`ApparentTemperature`)** und **Wochen-Heizzeitpläne (`HeatingSchedule`)** – laufen **ausschließlich auf dem CowHome-Backend (SQLite)**, ohne mit FritzBox-Vorlagen, FritzBox-Hardware-Gruppen oder internen FritzBox-Zeitplänen zu synchronisieren. Die FritzBox fungiert rein als physisches Funk-Gateway für Sensor-Telemetrie und direkte Aktor-Befehle.

## 0. Standardisiertes Capability-Modell (Digital Twin)

CowHome wertet die `functionbitmask` der FritzBox AHA-Schnittstelle aus und versieht jedes Gerät im Digital Twin (`GET /api/v1/state`) mit einer standardisierten `capabilities`-Liste:

| Geräteklasse | `device_type` | `capabilities`-Liste im Digital Twin | History-Spalten (`GET /api/v1/history`) |
| :--- | :--- | :--- | :--- |
| **FRITZ!DECT 301 / 302** | `thermostat` | `["thermostat", "temperature_sensor", "battery", "window_detection"]` | `["timestamp", "temperature_current", "temperature_target", "humidity", "battery_level", "window_open", "temperature_hardware", "hardware_target_temperature", "humidity_guard_active", "preheat_active"]` *(inkl. `efficiency` & `heating_forecast`)* |
| **Room Software Group (`group-{id}`)** | `thermostat_group` | *(Gruppen-Endpunkt & `/history?device=group-{id}`)* | `["timestamp", "temperature_current", "temperature_target", "humidity", "battery_level", "window_open", "temperature_hardware", "hardware_target_temperature", "humidity_guard_active", "preheat_active"]` *(inkl. `efficiency` & `heating_forecast`)* |
| **FRITZ!Smart Control 440** | `sensor` | `["temperature_sensor", "humidity_sensor", "battery", "button"]` | `["timestamp", "temperature_current", "humidity", "battery_level"]` |
| **FRITZ!DECT 200 / 210** | `switch` | `["switch", "power_meter", "temperature_sensor"]` | `["timestamp", "state", "power_current", "energy_total", "temperature_current", "voltage"]` *(inkl. `energy_summary`)* |
| **FRITZ!Smart Energy 250** | `switch` | `["power_meter", "battery"]` | `["timestamp", "state", "power_current", "energy_total", "temperature_current", "voltage"]` *(inkl. `energy_summary`)* |
| **Outdoor Weather (`WEATHER_01`)** | `weather` | *(nur History & `/weather`, nicht in Geräteliste)* | `["timestamp", "temperature", "apparent_temperature", "humidity", "wind_speed", "weather_code"]` |
| **FritzBox Gateway (`ROUTER_01`)** | `router` | `["router"]` | `["timestamp", "present"]` |

---

## 1. Schaltbare Steckdosen & Stromzähler-Sensoren (FRITZ!Smart Energy 200 / 210 / 250, DECT 200)

* **Auslesen:** `present`, `state`, `power_current` (W, negativ bei PV-Einspeisung), `voltage` (V), `energy_total` (Wh), `temperature_current` (°C), `battery_level` (%) & `battery_low` (über HAN-FUN Cross-Linking beim `Smart Energy 250`), `energy_summary` (`today_kwh`, `week_kwh`, `month_kwh`, `year_kwh`, `projected_month_kwh`, `projected_year_kwh` + EUR-Kosten & Tarif-Grundpreis-Umlage auf `"house"`).
* **Steuern:** `set_switch`, `toggle_switch`, `set_name`, `PowerSchedule` / `HeatingSchedule` (48 Slots/Tag).

## 2. Heizkörper-Thermostate (FRITZ!DECT 301 / FRITZ!Smart Thermo 302)

* **Auslesen:** `temperature_current` (Raum-Ist), `temperature_hardware` (Ventil-Ist), `temperature_target` (Wunsch-Soll), `hardware_target_temperature` (kompensierter Ventil-Sollwert), `temperature_offset` & `fritz_temperature_offset`, `comfort_temperature` & `eco_temperature`, `apparent_temperature`, `humidity` & DIN-4108-2-Felder (`dew_point_c`, `abs_humidity_gm3`, `wall_surface_humidity_pct`, `projected_base_humidity_pct`, `humidity_guard_active`, `humidity_delta_outdoor_gm3`, `ventilation_advice`), `battery_level`, `battery_low`, `window_open`, `boost_active`, `preheat_active`, `preheat_target_temp`, `preheat_target_time`, `preheat_paused_reason`, `error_code` (`0`–`6`) & `error_description`.
* **Steuern:** `set_temperature`, `set_comfort`, `set_eco`, `set_presets`, `set_offset` (`-5.0..+5.0 °C` in `0.5 °C`-Schritten, `409 Conflict` bei aktivem Raumfühler/Gruppe/Fritz-Offset), `set_name`, `HeatingSchedule`.

## 3. Wandtaster & Klimasensor (FRITZ!Smart Control 440)

* **Auslesen:** `temperature_current`, `humidity`, `apparent_temperature` & DIN-4108-2-Psychrometrie, `battery_level`, `battery_low`, `last_pressed`.
* **Master-Slave-Verknüpfung:** Zero-Config Auto-Binding in Software-Gruppen oder explizit via `sensor_ain` in `ApparentTemperature` / `HumidityControl`.

## 4. Weitere Smart-Home-Geräte (FRITZ!DECT 500, 350, Rollläden)

* **LED-Lampen (DECT 500):** `set_switch`, `set_level`, `set_color` (`hue`/`saturation`), `set_color_temperature` (`kelvin`, Alias `set_color_temp`).
* **Tür-/Fensterkontakte (DECT 350):** `window_detection` / `alarm_sensor`.
* **Rollläden (DECT ULE Partner):** `set_blind` (`open`, `close`, `stop`), `set_level` (`0`–`100`).

## 5. FritzBox Core-Funktionen (`ROUTER_01` via TR-064)

* **System, WLAN & WAN (`GET /api/v1/system/status`):** `router_info` (`model`, `manufacturer`, `software_version`, `serial_number`), `connection_status` (`is_linked`, `is_connected`, `uptime`, `external_ip`, `bytes_sent`, `bytes_received`), `wan_ip`, `wlan_main` (`enabled`, `ssid`, `channel`) & `wlan_guest` (`enabled`, `ssid`).
* **Aktionen auf `ROUTER_01`:** `reconnect_internet`, `reboot`, `set_wlan`, `set_guest_wlan`, `wake_on_lan`, `set_name`.
* **Hosts & Telefonie:** `GET /api/v1/system/hosts`, `GET /api/v1/system/calls` (`calls` + `answering_machine`).
