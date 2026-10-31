# gpu-tray

Ikona v oznamovací oblasti pro hlavní grafickou kartu (kartu s největší vyhrazenou pamětí, tady RX 9070 XT).

- **Ikona:** sloupec zaplněný podle obsazení VRAM. Zelená pod 70 %, oranžová pod 90 %, jinak červená.
- **Bublina:** obsazená VRAM, sdílená paměť v RAM a proces, který zabírá nejvíc.
- **Menu (klik na ikonu):**
  - procesy seřazené podle VRAM, s přetečením do RAM
  - restart ovladače grafiky
  - spouštění po přihlášení
  - konec

## Restart grafiky

Spouští `pnputil /restart-device <id karty>` s právy správce (okno UAC). Před spuštěním vypíše procesy,
které kartu používají. LM Studio a ComfyUI je lepší předem ukončit.

Vypnutí a zapnutí zařízení (`Disable-PnpDevice`) se záměrně nepoužívá: na běžící kartě 31. 10. 2026
shodilo celé Windows a karta zůstala vypnutá (kód 22).

## Sestavení a kontrola

```
cargo build --release
target\release\gpu-tray.exe                     # ikona
target\release\gpu-tray.exe --dump vystup.txt   # jednorázový výpis dat do souboru
```

Data pocházejí z výkonnostních čítačů Windows `GPU Process Memory` a `GPU Adapter Memory`
(nezávislé na výrobci) a z DXGI. LUID karty se po restartu mění, proto se karta hledá při každém čtení.
