# CS Translator

Live voice and text translation for Counter-Strike 2, powered by tiny AI models running on your own PC. Zero network requests, full privacy, and nothing to pay. Read what your teammates are saying in your chosen language without sending match audio or messages to a cloud service.

## Features

- **Voice and chat captions.** Translate game speech and in-game text chat into a single, readable feed, with original text available alongside translations.
- **Text-only mode.** Switch off voice translation from the main screen to stop audio capture, speech recognition, and speaker detection while keeping chat translation running.
- **A configurable desktop overlay.** Keep recent captions over a borderless or windowed game.
- **Automatic speaker-name detection.** Optionally read visible speaking indicators so your chat log shows who said what.
- **Local, offline processing.** Speech recognition, language identification, translation, and speaker reading run locally.

## Use-cases

- Translate written and spoken text into your language
- Keep a record of what was said in voice chat
- Voice chat subtitles

## Make it yours

- Choose the translation output language
- Show or hide original text and message labels and adjust caption font size.
- Tune the overlay's position, text size, transparency, and message lifetime.
- Optionally add speaker-name labels to voice captions.
- Set CPU inference threads, background priority, translation decoding, and partial-update frequency.
- Preview caption and overlay styling directly in Settings.

## Local AI models

| Purpose | Model | Runtime |
| --- | --- | --- |
| Speech recognition | Nemotron 3.5 multilingual streaming, 0.6B INT8 | sherpa-onnx, CPU |
| Translation | M2M100 418M INT8 | CTranslate2, CPU |
| Language identification | fastText `lid.176.ftz` | fastText |
| Speaker-label reading | PaddleOCR PP-OCRv5 mobile, Latin/common and Cyrillic recognizers | ONNX Runtime, CPU |

Models are downloaded separately (but automatically) from the app and stored in the `models/` folder beside it.

| Setup mode | Model download | Installed model files |
| --- | --- | --- |
| Text only | Approximately 497 MB | Approximately 497 MB |
| Voice and text | Approximately 1.075 GB | Approximately 1.22 GB |

## Get started

1. Download the **Windows ZIP** from **GitHub Releases**, extract it into a writable folder, and open **cs-translator.exe**. Keep the executable, DLLs, and `model-manifests/` folder together.
2. Download the required models in the welcome screen. Turn off **Voice translation** if you only want text chat.
3. For chat translation, add **`-condebug`** to CS2's Steam launch options, preserving existing options, and restart CS2 if it was already running. The app walks you through this step.
4. Choose your output language, launch CS2 normally, and press **Start**. Enable the overlay or speaker detection in Settings if desired.

## Requirements and expectations

**Windows 11 x64** is currently supported. A current Microsoft Visual C++ v14 x64 runtime is required. If it isn't already installed, run the included **prerequisites/vc_redist.x64.exe** once, then open **cs-translator.exe**. Windows may request permission or a restart during runtime installation.

Speech capture receives CS2's mixed audio output, not an isolated voice-chat channel. Noise, accents, short callouts, and overlapping voices can affect recognition. Player labels can be missed or misread. Overlay visibility over exclusive fullscreen is not possible.

Performance may be a problem depending on your system. I have tested this on my AMD 9800X3D and see no visible performance impact, but these processes are relatively heavy, so lower-spec systems may suffer. YMMV.

There should be no anti-cheat concerns: The app uses console logs, documented Windows capture APIs (like OBS etc. would), and an independent desktop window. No game injection, game-memory access, graphics/input hooks, or Trusted Mode changes.

Third-party licenses, model terms, and package provenance are included with the Windows download and accessible through the app's license browser.

## Releases

GitHub Actions verifies Windows builds on branch pushes and pull requests. To publish a release, push a version tag matching `workspace.package.version` in `Cargo.toml`, such as `v0.1.0`. The release workflow can also be run manually for an existing version tag.

Each release includes the Windows x64 ZIP and its SHA-256 checksum. The bundled Microsoft Visual C++ installer is hash- and signature-verified before packaging. Model weights are downloaded in the app, not bundled in releases.
