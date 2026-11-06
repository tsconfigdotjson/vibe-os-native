<p align="center">
  <img src="swift/VibeOS/Support/AppIcon.iconset/icon_128x128@2x.png" width="112" height="112" alt="The VibeOS app icon">
</p>

<h1 align="center">VibeOS Native</h1>

<p align="center"><strong>Cloud agents on your Mac and VPS.</strong></p>

<p align="center">
  Hand coding work to agents that run on machines you already own.<br>
  Watch them from the menu bar, jump in when they need you, and keep going from a compatible assistant.
</p>

<p align="center">
  <a href="https://github.com/tsconfigdotjson/vibe-os-native/releases/latest"><strong>Download for Mac</strong></a>
  &nbsp;·&nbsp; Apple Silicon, macOS 26 or later &nbsp;·&nbsp; Free and open source (MIT)
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/menu-dark.png">
    <img src="docs/images/menu-light.png" width="320" alt="VibeOS menu showing demo agents working on a Mac and Linux servers">
  </picture>
</p>

<p align="center"><sub>Screenshots are the real app on a fictional demo workspace (Larkfield Labs).</sub></p>

## Why people use it

### Hand off work, stay in control
Give an agent a goal and let it run on your Mac or a Linux server you add. You still own the machines, the code, and the accounts.

### Open a real terminal anytime
Click an agent and Terminal opens attached to its live session. Close the window or step away — the agent keeps going. Come back and the screen is still there.

### Drive it from an assistant you already use
Compatible MCP clients (for example Claude Code) can start agents, wait for them, and answer questions. One API key. Loopback by default; optional private Tailscale when you want another machine to reach it.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/new-agent-dark.png">
    <img src="docs/images/new-agent-light.png" width="400" alt="New Agent sheet to start work on a project">
  </picture>
</p>

## Get started

1. **[Download the latest release](https://github.com/tsconfigdotjson/vibe-os-native/releases/latest)** and open the DMG.
2. Drag VibeOS into Applications and launch it. Follow the welcome checklist.
3. Add a project, create an API key in **Settings → API Keys**, and start an agent from the menu or your assistant.

That's enough for day one. Deeper networking, development, and design notes live in the docs below.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/settings-hosts-dark.png">
    <img src="docs/images/settings-hosts-light.png" width="520" alt="Settings Hosts pane with this Mac and connected Linux servers">
  </picture>
</p>

## Good to know

- Agents run on **your** Mac and optional Linux servers — not a VibeOS cloud.
- Keep API keys private; a key can act with your powers on those machines.
- A sleeping Mac pauses agents on it; a Linux server can keep its own agents going while the Mac is away.

## Learn more

- [MCP and networking](docs/mcp-and-networking.md) — keys, addresses, Tailscale (optional), limits
- [Developing VibeOS](docs/development.md) — build, check, release
- [PRD.md](PRD.md) — full design
- [Contributing](CONTRIBUTING.md) · [Issues](https://github.com/tsconfigdotjson/vibe-os-native/issues)

VibeOS Native is released under the [MIT License](LICENSE).
