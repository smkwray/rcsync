<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import type { AppConfig } from "./lib/types";
  import Dashboard from "./lib/Dashboard.svelte";
  import Settings from "./lib/Settings.svelte";
  import BrowseRemote from "./lib/BrowseRemote.svelte";
  import logo from "./assets/logo.png";

  let showSettings = $state(false);
  let showBrowse = $state(false);
  let settingsConfig: AppConfig | null = $state(null);
  let theme = $state(localStorage.getItem("rcsync-theme") || "dark");

  document.documentElement.setAttribute("data-theme", theme);

  function toggleTheme() {
    theme = theme === "dark" ? "light" : "dark";
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("rcsync-theme", theme);
  }

  async function openSettings() {
    settingsConfig = await invoke<AppConfig>("get_config");
    showSettings = true;
  }

  function closeSettings() {
    showSettings = false;
    settingsConfig = null;
    dashboardKey = {};
  }

  function openBrowse() {
    showBrowse = true;
  }

  function closeBrowse() {
    showBrowse = false;
    dashboardKey = {};
  }

  let dashboardKey = $state({});

  // Listen for keyboard shortcut events from Dashboard
  if (typeof window !== "undefined") {
    window.addEventListener("open-settings", () => openSettings());
    window.addEventListener("open-browse", () => openBrowse());
    window.addEventListener("close-overlays", () => {
      if (showSettings) { closeSettings(); }
      else if (showBrowse) { closeBrowse(); }
    });
  }
</script>

<div class="app">
  <nav class="topbar">
    <div class="nav-left">
      <img src={logo} alt="rcsync" class="logo" />
      <span class="brand">rcsync</span>
    </div>
    <div class="nav-right">
      <button class="nav-btn" onclick={openBrowse}>Browse Remote</button>
      <button class="nav-btn icon-btn" onclick={toggleTheme} title="Toggle theme">
        {#if theme === "dark"}
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="5"/><line x1="12" y1="1" x2="12" y2="3"/><line x1="12" y1="21" x2="12" y2="23"/><line x1="4.22" y1="4.22" x2="5.64" y2="5.64"/><line x1="18.36" y1="18.36" x2="19.78" y2="19.78"/><line x1="1" y1="12" x2="3" y2="12"/><line x1="21" y1="12" x2="23" y2="12"/><line x1="4.22" y1="19.78" x2="5.64" y2="18.36"/><line x1="18.36" y1="5.64" x2="19.78" y2="4.22"/>
          </svg>
        {:else}
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>
          </svg>
        {/if}
      </button>
      <button class="nav-btn" onclick={openSettings}>Settings</button>
    </div>
  </nav>

  {#key dashboardKey}
    <Dashboard />
  {/key}

  {#if showSettings && settingsConfig}
    <Settings config={settingsConfig} onclose={closeSettings} />
  {/if}

  {#if showBrowse}
    <BrowseRemote onclose={closeBrowse} />
  {/if}
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  .topbar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 8px 20px;
    border-bottom: 1px solid var(--border);
    background: var(--bg-card);
    -webkit-app-region: drag;
  }

  .topbar button {
    -webkit-app-region: no-drag;
  }

  .nav-left {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .logo {
    width: 28px;
    height: 28px;
    object-fit: contain;
  }

  .brand {
    font-size: 14px;
    font-weight: 700;
    letter-spacing: -0.3px;
    color: var(--accent);
  }

  .nav-right {
    display: flex;
    gap: 8px;
    align-items: center;
  }

  .nav-btn {
    font-size: 12px;
    padding: 4px 12px;
  }

  .icon-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 5px 8px;
  }

  .icon-btn svg {
    display: block;
  }
</style>
