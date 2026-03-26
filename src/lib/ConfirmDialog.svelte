<script lang="ts">
  let {
    title,
    message,
    confirmLabel = "Confirm",
    danger = false,
    onconfirm,
    oncancel,
  }: {
    title: string;
    message: string;
    confirmLabel?: string;
    danger?: boolean;
    onconfirm: () => void;
    oncancel: () => void;
  } = $props();

  let cancelBtn: HTMLButtonElement | undefined = $state(undefined);

  // Focus Cancel by default so Enter = No
  $effect(() => { cancelBtn?.focus(); });

  function handleKey(e: KeyboardEvent) {
    if (e.key === "Escape") { e.preventDefault(); oncancel(); }
  }
</script>

<div class="overlay" role="dialog" onkeydown={handleKey}>
  <div class="dialog">
    <h3>{title}</h3>
    <p class="msg">{@html message.replace(/\n/g, "<br>")}</p>
    <div class="actions">
      <button bind:this={cancelBtn} class="cancel-btn" onclick={oncancel}>Cancel</button>
      <button class={danger ? "danger" : "primary"} onclick={onconfirm}>{confirmLabel}</button>
    </div>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 300;
  }

  .dialog {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 24px;
    max-width: 420px;
    width: 90%;
  }

  h3 {
    font-size: 15px;
    font-weight: 700;
    margin-bottom: 10px;
  }

  .msg {
    font-size: 13px;
    color: var(--text-muted);
    line-height: 1.6;
    margin-bottom: 18px;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .cancel-btn {
    font-weight: 600;
  }

  .cancel-btn:focus {
    box-shadow: 0 0 0 2px var(--accent);
    outline: none;
  }
</style>
