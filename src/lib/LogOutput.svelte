<script lang="ts">
  let { lines = $bindable([]) }: { lines: string[] } = $props();
  let container: HTMLDivElement;

  $effect(() => {
    if (lines.length && container) {
      container.scrollTop = container.scrollHeight;
    }
  });
</script>

<div class="log-output" bind:this={container}>
  {#each lines as line}
    <div class="log-line">{line}</div>
  {/each}
  {#if lines.length === 0}
    <div class="log-empty">No output yet.</div>
  {/if}
</div>

<style>
  .log-output {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 12px;
    font-family: var(--font-mono);
    font-size: 12px;
    line-height: 1.6;
    overflow-y: auto;
    flex: 1;
    min-height: 0;
  }

  .log-line {
    white-space: pre-wrap;
    word-break: break-all;
    color: var(--text-muted);
  }

  .log-empty {
    color: var(--text-muted);
    font-style: italic;
  }
</style>
