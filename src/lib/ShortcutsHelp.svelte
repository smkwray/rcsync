<script lang="ts">
  let { onclose }: { onclose: () => void } = $props();

  const groups = [
    {
      title: "Navigation",
      keys: [
        ["j", "Down"],
        ["k", "Up"],
        ["l", "Left"],
        [";", "Right"],
        ["/", "Focus filter"],
      ],
    },
    {
      title: "Selected Card",
      keys: [
        ["a", "Push"],
        ["s", "Dry Run"],
        ["d", "Check"],
        ["f", "Bi-Sync"],
        ["g", "Pull"],
        ["h", "Delete local"],
      ],
    },
    {
      title: "Global (Keys on)",
      keys: [
        ["c", "Check All"],
        ["p", "Push All"],
        ["o", "Toggle output"],
        ["x", "Clear Log"],
        ["b", "Browse Remote"],
        ["Esc", "Deselect / close"],
      ],
    },
    {
      title: "Always On",
      keys: [
        ["\u2318,", "Settings"],
        ["\u2318K", "Toggle shortcuts"],
        ["\u2318O", "Toggle output"],
        ["?", "This help"],
        ["Esc", "Close overlay"],
      ],
    },
  ];
</script>

<div class="overlay" role="dialog" onclick={onclose} onkeydown={(e) => e.key === "Escape" && onclose()}>
  <div class="help" onclick={(e) => e.stopPropagation()}>
    <div class="help-header">
      <h2>Keyboard Shortcuts</h2>
      <button onclick={onclose}>Close</button>
    </div>
    <div class="help-body">
      {#each groups as group}
        <div class="group">
          <h3>{group.title}</h3>
          {#each group.keys as [key, desc]}
            <div class="row">
              <kbd>{key}</kbd>
              <span>{desc}</span>
            </div>
          {/each}
        </div>
      {/each}
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
    z-index: 200;
  }

  .help {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    width: 480px;
    max-height: 80vh;
    display: flex;
    flex-direction: column;
  }

  .help-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 14px 20px;
    border-bottom: 1px solid var(--border);
  }

  h2 { font-size: 15px; font-weight: 700; }

  .help-body {
    padding: 16px 20px;
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px;
    overflow-y: auto;
  }

  .group h3 {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--text-muted);
    margin-bottom: 6px;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 4px;
    font-size: 12px;
  }

  kbd {
    display: inline-block;
    min-width: 22px;
    padding: 2px 6px;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 4px;
    font-family: var(--font-mono);
    font-size: 11px;
    text-align: center;
    color: var(--text);
  }
</style>
