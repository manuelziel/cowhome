# CowHome - Automatisierung & Rules-Engine-Konzept

CowHome implementiert **7 parametrisierte Smart Features**, die zu 100 % autark auf dem Rust-Backend (`SQLite` + `StateRegistry`) ohne Synchronisation mit internen FritzBox-Zeitplänen oder Hardware-Gruppen ausgeführt werden.

---

## 1. Die 7 Smart Features im Überblick

1. **`WeatherSmartHeating` (`Summer Pause`):** Pausiert das Heizen auf `8.0 °C` innerhalb von `[start_date, end_date]` (Standard `"05-01"`..`"09-30"`), sobald `max_outdoor_temp >= outdoor_temp_threshold` (Standard `16.0 °C`), inkl. automatischer Kälte-Ausnahme im Sommer.
2. **`HumidityControl` v2 (`Humidity Guard` nach DIN 4108-2):** 4-Zustandsautomat (`IDLE`, `TRANSIENT_DEBOUNCE`, `THERMAL_SHIELDING`, `RECOVERY_CHECK`) mit Wärmebrücken-Faktor $f_{\text{Rsi}} = 0{,}70$, `20 Min.` Transienten-Debounce für Dusch-/Kochspitzen, bedarfsgenauer Schutztemperatur $T_{\text{guard}}$ (gedeckelt durch `max_temp_boost_c` und `23.0 °C`; `0.0 °C` = `Alert only`), pendelfreier Hysterese über $\varphi_{\text{proj}}(T_{\text{base}}) \le \text{threshold} - 4{,}0\,\%$ sowie Opt-In-Lüftungsempfehlung `ventilation_advice` (`"none"`, `"ventilate_now"`, `"ventilate_marginal"`, `"do_not_ventilate_humid_outside"`).
3. **`VacationMode` (`Frost Guard`):** Globaler Frostschutz (`8.0 °C`) mit `ignored_devices`/`ignored_groups`, automatischer Freigabe `pre_heat_hours` vor `end_date_timestamp` und Deaktivierung bei Rückkehr.
4. **`ApparentTemperature` (`Room Sensor Sync`) & `set_offset`:** Verknüpft Raumklimasensoren (`FRITZ!Smart Control 440`, inkl. Zero-Config Auto-Binding in Gruppen) als Master-Sensor und regelt `hardware_target_temperature` dynamisch inkl. Überhitzungsschutz nach:
   $$\text{Offset}_{\text{eff}} = \begin{cases} T_{\text{hw}} - T_{\text{room}} & \text{falls } T_{\text{room}} < T_{\text{target}} \\ \min(T_{\text{hw}} - T_{\text{room}},\; T_{\text{target}} - T_{\text{room}}) & \text{falls } T_{\text{room}} \ge T_{\text{target}} \end{cases}$$
   Für alleinstehende Thermostate ohne Raumfühler (`fritz_temperature_offset == 0.0`) verwaltet `set_offset` (`-5.0..+5.0 °C` in `0.5 °C`-Schritten) statisch `temperature_current = temperature_hardware + temperature_offset` und `hardware_target_temperature = clamp(temperature_target - temperature_offset, 8.0, 28.0)`.
5. **`HeatingSchedule` / `PowerSchedule`:** Flankengesteuerter 7-Tage-/30-Minuten-Wochenplan (`mon`–`sun`, 48 Slots/Tag) in der IANA-Hauszeitzone (`chrono-tz`, Standard `"Europe/Berlin"`) für Thermostate (`comfort_temp` / `eco_temp` + `preheat_enabled`) und Steckdosen (`EIN`/`AUS`), mit mitternachtssicherer `slot_id`, 90s `target_hold_until` und Respektierung manueller Overrides bis zum nächsten Phasenwechsel.
6. **`AdaptiveStart` (`Pre-Heat` nach DIN EN 12098-1 / EN 15232):** Lernt die totzeitbereinigte Aufheizrate $r_{\text{base}}$ (`15.0–120.0 min/°C`, Kesselpausen-Filter $> 25\,\text{min}$ über `temperature_hardware`), kompensiert Außentemperatur-Trägheit ($+2{,}5\,\%/\text{K}$ unter $10\,^\circ\text{C}$, Faktor `0.70..1.60` $\implies r_{\text{eff}}$), startet ab $\Delta T_{\text{in}} \ge 0{,}5\,\text{K}$ um $t_{\text{lead}} = \min(180, \text{round}(\Delta T_{\text{in}} \cdot r_{\text{eff}}))$ vor kommenden Komfort-Blöcken und publiziert `preheat_active`, `preheat_target_temp`, `preheat_target_time` und `preheat_paused_reason`.
7. **`PresenceProfiling` (`Auto-Away`):** Senkt die Solltemperatur um `away_temp_offset` ab, sobald alle überwachten WLAN-MAC-Adressen länger als `timeout_minutes` offline sind (`presence_history`).

---

## 2. Single-Authority Scope-Hierarchie & 5-Stufen-Prioritäts-Pyramide

* **Single Authority:** Ein Gerät kann in maximal einer Software-Gruppe sein (`UNIQUE(ain)`). Sobald ein Gerät gruppiert ist, gilt `automation_locked = true` (`managed_by = "<Gruppenname>"`) und ausschließlich das Gruppen-Profil steuert das Gerät.
* **5-Stufen-Priorität:**
  1. **Stufe 1 (Basis-Fahrplan):** `HeatingSchedule` / `PowerSchedule` (flankengesteuert).
  2. **Stufe 2 (Zeitliche Vorverlegung):** `AdaptiveStart` (`Pre-Heat`), koordiniert über $T_{\text{eff}} = \max(T_{\text{guard}}, T_{\text{preheat}})$ mit `HumidityControl`.
  3. **Stufe 3 (Situative Korrekturen):** `PresenceProfiling`, `WeatherSmartHeating`, `HumidityControl` v2.
  4. **Stufe 4 (Globale Schutz-Sperre):** `VacationMode` (+ Hardware `window_open`).
  5. **Stufe 5 (Physische Ventil-Ausführung):** `ApparentTemperature` & `set_offset` (`temperature_target` → `hardware_target_temperature`).
