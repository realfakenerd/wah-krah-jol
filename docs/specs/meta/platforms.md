# Mudcrab Cross-Platform Target Architecture & Strategy

This document outlines the multi-platform target strategy for Mudcrab, ensuring native performance across **Desktop (Windows, Linux, macOS)** and **Mobile/ARM (Android, iOS, iPadOS)**.

---

## 1. Supported Target Matrix

| Platform         | Primary Graphics API   | CPU Architecture          | Build Target            | Input Methods                      |
| :--------------- | :--------------------- | :------------------------ | :---------------------- | :--------------------------------- |
| **Windows**      | Vulkan / DirectX 12    | `x86_64`                  | Native `.exe`           | Keyboard/Mouse, Gamepad            |
| **Linux**        | Vulkan                 | `x86_64` / `aarch64`      | Native Binary / Flatpak | Keyboard/Mouse, Gamepad            |
| **macOS**        | Metal (via WGPU)       | `aarch64` (Apple Silicon) | Native `.app` bundle    | Keyboard/Mouse, Gamepad            |
| **Android**      | Vulkan / OpenGL ES 3.2 | `aarch64` (ARM64)         | Native `.apk` / `.aab`  | On-screen Touch, Bluetooth Gamepad |
| **iOS / iPadOS** | Metal                  | `aarch64`                 | Native `.ipa` bundle    | On-screen Touch, MFi Gamepad       |

---

## 2. Architectural Pillars for Cross-Platform Success

### A. Bevy & WGPU (Graphics Abstraction)

- Bevy uses **`wgpu`** as its rendering engine backend.
- `wgpu` automatically targets **Vulkan** (Windows/Linux/Android), **Metal** (macOS/iOS), and **DirectX 12** (Windows) without writing separate rendering code for each OS.

### B. Asset Pipeline Uniformity (glTF 2.0 & KTX2)

- Current KTX2 output mixes native BC preservation, CPU BC7 encoding and UASTC fallback. Packs containing BC need compatible GPU support; converting to KTX2/glTF does not establish platform-independent loading.
- Bevy can select ASTC, BC7 or ETC2 for UASTC based on device capabilities. Native BC payloads do not use that transcode path. Mobile/web pack loading remains unproven and requires a portable texture profile plus target validation. [ADR-0014](../../adr/0014-phase1-desktop-texture-contract.md) proposes deferring that work; the maintainer decision and implementation/validation tracking remain open in [#274](https://github.com/Mudcrab-Team/mudcrab/issues/274).

### C. Touch & Gamepad Responsive Controls

- Mudcrab's UI engine incorporates an **adaptive touch overlay system** for Android & iOS.
- Automatic input device switching:
  - Mouse/Keyboard ➔ Xbox/PlayStation Controller ➔ On-screen Virtual Joystick.

---

## 3. Platform-Specific Considerations

### 📱 Android (`aarch64-linux-android`)

- Built using `cargo-apk` / `ndk-build`.
- Uses Android Storage Access Framework (SAF) for picking Skyrim installation files during launcher setup.
- Memory management: KTX2 texture streaming prevents out-of-memory (OOM) crashes on mobile devices with 4GB - 6GB RAM.

### 🍏 macOS & iOS (`aarch64-apple-darwin` / `aarch64-apple-ios`)

- Fully native on Apple Silicon (M1/M2/M3/M4) chips via Metal.
- iOS builds compile with touch UI overlays and MFi gamepad support.

### 🐧 Linux (`x86_64-unknown-linux-gnu`)

- Zero dependency on Wine/Proton. Native Linux binary compiled with Vulkan support.
- Portable distribution via AppImage and Flatpak.
