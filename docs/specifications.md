# CowHome - Projektspezifikation

## Referenzen & Externe Dokumentation
- **AVM Entwickler-Schnittstellen (Offiziell):** [avm.de/service/schnittstellen/](https://avm.de/service/schnittstellen/)
- **AHA-HTTP-Interface-Spezifikation (PDF):** [AHA-HTTP-Interface.pdf](https://avm.de/fileadmin/user_upload/Global/Service/Schnittstellen/AHA-HTTP-Interface.pdf)
- **TR-064-Schnittstelle (PDF):** [TR-064-firststeps.pdf](https://avm.de/fileadmin/user_upload/Global/Service/Schnittstellen/TR-064-firststeps.pdf)


## 1. Projektübersicht & Vision
* **Projektname:** CowHome
* **Externe Domain:** `home.libracow.com`
* **Zielsetzung:** CowHome ist nicht nur eine Schnittstelle, sondern ein intelligenter **Smart Home Hub**. Es fungiert als "Single Source of Truth" (Digitaler Zwilling).
* **Architektur-Fokus (100 % Standalone, Single-Binary Rust & Backend-Autarkie):**
  * **Keine Cloud-/SSO-Abhängigkeit:** Es gibt keine Abhängigkeiten zu externen Login-Servern (wie CowAPI). Das System kann problemlos geklont und bei Familie/Freunden unabhängig betrieben werden.
  * **Alles läuft ausschließlich auf dem CowHome-Backend (keine FritzBox-Synchronisation):** Die FritzBox dient ausschließlich als physisches Funk-Gateway (DECT-ULE/HAN-FUN) zum Auslesen roher Hardware-Messwerte und zum Senden direkter Schalt-/Temperaturbefehle (`sethkrtsoll`). Sämtliche Logik, **Gerätenamen (`custom_names`)**, **virtuelle Software-Gruppen**, **Komfort- und Absenktemperaturen (`comfort_temperature` / `eco_temperature`)**, **Raumfühler-Abhängigkeiten (`ApparentTemperature`)** sowie **Wochen-Heizzeitpläne (`HeatingSchedule`)** werden zu **100 % autark im CowHome-Backend (SQLite)** verwaltet und niemals mit FritzBox-Vorlagen, FritzBox-Hardware-Gruppen oder internen FritzBox-Zeitplänen synchronisiert.
* **Erweiterungen:** Zieht externe Daten (z.B. lokales Wetter) für intelligente Entscheidungen heran.

## 2. Systemdesign ("Big-Project"-Architektur)
Große Projekte wie *Home Assistant* oder *openHAB* nutzen das Konzept eines **State-Managers (Digital Twin)**. CowHome adaptiert dieses Prinzip als extrem ressourcenschonende, statisch gelinkte Rust-Single-Binary (`axum` + `tokio` + `rusqlite`):

1. **Single Source of Truth (Status-Speicher):**
   * CowHome speichert den aktuellen Zustand aller Geräte im RAM (`StateRegistry`) und in einer lokalen SQLite-Datenbank.
   * **Mobile Apps (Clients)** greifen *ausschließlich* auf CowHome zu. Sie fragen nicht die FritzBox ab, sondern laden das aktuelle "Abbild" (State) von CowHome (sehr schnell, keine Latenz).
   * Wenn die App eine Änderung fordert (z.B. Heizung auf 22 °C), speichert CowHome dies als "Soll-Zustand" und kümmert sich im Hintergrund um die Übertragung zur FritzBox.

2. **Background Polling & Sync (Der Worker):**
   * Ein Hintergrund-Prozess fragt die FritzBox zyklisch (z.B. alle 1–5 Minuten) nach physischen Hardware-Änderungen (z.B. ob jemand manuell am Thermostat-Drehrad gedreht hat). Virtuelle FritzBox-Hardware-Gruppen (`<group>` / `is_group`) und FritzBox-interne Komfort-/Absenk-Tags werden dabei konsequent ignoriert.

3. **History & Time-Series Data (Langzeit-Aufzeichnung mit Deadband-Kompression, Heartbeat & Pre-Edge-Stützpunkt) & Offline-First:**
   * **Aufzeichnung (Deadband-Filter + 30-Minuten-Heartbeat + Pre-Edge Anchor):** CowHome speichert historische Snapshots in SQLite (`state_history`) nach dem Industriestandard (analog zu *Home Assistant Recorder* / *InfluxDB*):
     * *Deadband-Filter (Change-Only):* Ein Punkt wird sofort geschrieben bei jedem diskreten Statuswechsel (`present`, `state`, `window_open`, `boost_active`, `battery_low`, `error_code`, `last_pressed`, `humidity_guard_active`, `preheat_active`) oder sobald ein Messwert sein Deadband überschreitet ($|\Delta T_{\text{current}}| \ge 0{,}1\,^\circ\text{C}$, $|\Delta T_{\text{hardware}}| \ge 0{,}1\,^\circ\text{C}$, $|\Delta T_{\text{target}}| \ge 0{,}5\,^\circ\text{C}$, $|\Delta \text{Humidity}| \ge 1{,}0\,\%$, $|\Delta \text{Battery}| \ge 1{,}0\,\%$, $|\Delta U| \ge 3{,}0\text{ V}$, $|\Delta E_{\text{total}}| \ge 10{,}0\text{ Wh}$, $|\Delta P| \ge \max(0{,}5\text{ W},\ 2\,\%)$).
     * *30-Minuten-Heartbeat & Pre-Edge Anchor („Step-Before"-Stützpunkt):* Bleiben alle Werte konstant, schreibt CowHome spätestens alle `30 Minuten` (`1800 s`) einen Heartbeat-Punkt. Tritt nach unterdrückten Polls ($\ge 120\text{ s}$) eine signifikante Änderung auf, wird unmittelbar vor der Flanke automatisch der letzte unterdrückte Zustand mit seinem historischen Poll-Zeitstempel als Stützpunkt eingefügt, damit `MPAndroidChart` exakte Stufen statt stundenlanger Diagonalen darstellt. Dasselbe Verfahren gilt auch für `presence_history`.
     * *Flash-/SSD-Schonung:* Zero-Write Entity-Cache, LWW Zero-Write Upsert-Guards (`WHERE ... IS NOT excluded...`), gebündelte Sammel-Transaktionen pro Poll-Zyklus und SQLite-Wear-Leveling-PRAGMAs (`journal_mode = WAL`, `synchronous = NORMAL`, `temp_store = MEMORY`, `journal_size_limit = 64 MiB`, `cache_size = -16384` [16 MiB RAM], `mmap_size = 256 MiB`).
   * **Metriken:** Temperatur-Verlauf (Ist/Soll), Luftfeuchtigkeit (%), Batteriestand, Fenster-Status, Schaltzustände von Steckdosen, Wirkleistung (Watt), Netzspannung (Volt), kumulierter Energieverbrauch (Wh/kWh) sowie das lokale Außenwetter (`WEATHER_01`).
   * **Offline-First Synchronisations-Protokoll:**
     * *Initial-Sync:* `GET /api/v1/history?device={ain}&range=30d` (oder `range=all&limit=0`).
     * *Inkrementeller Delta-Sync:* `GET /api/v1/history?device={ain}&since={unix_timestamp}`.
     * *Client-Side Downsampling:* Rohdaten im kompakten spaltenbasierten Array-of-Arrays-Format (`columns` + `points`).
   * **Heizeffizienz (`efficiency`), 5-Tages-Heiz-Tendenz (`heating_forecast`) & Smart Energy (`energy_summary`):** Totzeitbereinigte Aufheizrate (`heating_rate_min_per_c` in `15.0–120.0 min/°C`), wetterkompensierte Trägheit (`effective_heating_rate_min_per_c`), `heating_rate_c_per_hour`, 5-Tage-Heizprognose nach VDI 3807 / VDI 2067 sowie Smart-Energy-Verbrauch & Kostenprognose inkl. zeitanteiliger Grundpreis-Umlage auf `device == "house"` und Priorisierung optischer Hauptzähler (`FRITZ!Smart Energy 250`).

4. **Software-Gruppen mit Auto-Sync (CowHome als Master-Controller):**
   * Raumgruppen werden zu 100 % in SQLite verwaltet (`software_groups`, `software_group_members`). Manuelle App-Aktionen sowie physische Drehrad-Änderungen synchronisieren automatisch alle Gruppenmitglieder.

5. **Storage Diagnostics & Universal System Health:**
   * Überwacht SQLite-Dateigrößen (`cowhome.db`, `-wal`, `-shm`), Zeilenzahlen pro Tabelle und Host-Speicherplatz (`statvfs`) über `GET /api/v1/system/storage` sowie den 5-Block-Gesundheitsbericht nach *Universal Diagnostics Schema v1.0* über `GET /api/v1/system/diagnostics` (`/ai-diagnostics`). Tägliches Auto-Pruning um `00:00 Uhr` Lokalzeit inkl. `PRAGMA wal_checkpoint(PASSIVE)`.

6. **Wetter-Integration & Outdoor Climate Engine (Open-Meteo):**
   * Abruf von `current`, 2h-`hourly`-Slots und 5-Tage-`daily`-Prognose sowie Standort-Zeitzone (`chrono-tz`, Default `"Europe/Berlin"`), Persistenz unter `WEATHER_01`.

7. **Multi-Device Offline-Sync & Out-of-Order-Schutz:**
   * Last-Write-Wins (`timestamp` / `client_timestamp` / `X-Client-Timestamp`), Clock-Skew-Clamping (`min(client_ts, server_now)`) und Command TTL (`300 s` für direkte Schalt-/Router-Befehle).

## 3. Intelligente Automatisierung & Logik
CowHome implementiert **7 parametrisierte Smart Features** in einer deterministischen 5-Stufen-Prioritäts-Pyramide:
1. **`HeatingSchedule` / `PowerSchedule`** (7 Tage × 48 Slots in Haus-Zeitzone inkl. `preheat_enabled`)
2. **`WeatherSmartHeating`** (Sommerpause mit Kälte-Ausnahme)
3. **`HumidityControl` v2** (DIN 4108-2 Schimmelschutz & Außenklima-Lüftungsempfehlung)
4. **`VacationMode`** (Urlaubs-Frostschutz & automatisches Vorheizen vor Rückkehr)
5. **`PresenceProfiling`** (TR-064 WLAN-Auto-Away mit Karenzzeit)
6. **`ApparentTemperature` & `set_offset`** (`FRITZ!Smart Control 440` Master-Slave-Nachführung & Single Source of Calibration)
7. **`AdaptiveStart`** (DIN EN 12098-1 Pre-Heat aus gelernter Aufheizrate)

➡️ Vollständige Spezifikation, Formeln & Scope-Matrix: [`docs/automation_rules.md`](automation_rules.md)

## 4. API & Sicherheit
* **Web-Stack:** `axum` + `tokio` (`max_body_size = 1 MiB`), eingebettetes `assets/openapi.yaml` und `assets/swagger.html` (`/api/v1/docs`).
* **Authentifizierung:** Strikte Header-Only API-Key-Pflicht nach `RFC 6750` (`Authorization: Bearer`, `Authorization: ApiKey`, `X-API-Key`), Fail-Closed Key-Policy ($\ge 16$ Zeichen, keine Platzhalter), Constant-Time-Vergleich, Trusted-Proxy-IP-Auflösung und 2-Stufen `RateLimiter` (`120 req/min` + `5` Auth-Fails/Stunde).
