<script lang="ts">
  let { lines = $bindable([]) }: { lines: string[] } = $props();
  let container: HTMLDivElement;

  /** Within this many pixels of the end counts as "at the bottom". Exact
   *  equality never holds: sub-pixel layout and a fractional device pixel ratio
   *  leave scrollTop a hair short of the maximum even when the view is pinned. */
  const BOTTOM_SLACK = 24;

  /** Whether new output should pull the view down with it. Live output arrives
   *  every few hundred milliseconds now, so unconditional auto-scroll — what
   *  this did before — would yank the view back down every time the user
   *  scrolled up to read something. */
  let follow = $state(true);
  let unseen = $state(0);

  function atBottom(): boolean {
    if (!container) return true;
    return container.scrollHeight - container.scrollTop - container.clientHeight <= BOTTOM_SLACK;
  }

  export function scrollToBottom() {
    follow = true;
    unseen = 0;
    if (container) container.scrollTop = container.scrollHeight;
  }

  function onScroll() {
    // Leaving the bottom detaches; returning to it re-attaches, so the common
    // case (scroll up, read, scroll back down) needs no button press.
    follow = atBottom();
    if (follow) unseen = 0;
  }

  let lastCount = 0;
  $effect(() => {
    const count = lines.length;
    if (count === lastCount) return;
    // A cleared log resets everything, including a detached view.
    if (count < lastCount) {
      lastCount = count;
      scrollToBottom();
      return;
    }
    const added = count - lastCount;
    lastCount = count;
    if (follow) {
      if (container) container.scrollTop = container.scrollHeight;
    } else {
      unseen += added;
    }
  });
</script>

<div class="log-wrap">
  <div class="log-output" bind:this={container} onscroll={onScroll}>
    {#each lines as line}
      <div class="log-line">{line}</div>
    {/each}
    {#if lines.length === 0}
      <div class="log-empty">No output yet.</div>
    {/if}
  </div>

  {#if !follow}
    <button class="jump" onclick={scrollToBottom} title="Scroll to the newest output">
      {unseen > 0 ? `${unseen} new` : "Latest"} &#x25BC;
    </button>
  {/if}
</div>

<style>
  /* Positioning context for the jump button, and the flex parent that lets the
     scroller take a bounded height. Without the bounded height the scroller
     grows to fit its content, never overflows, and scrolling it does nothing —
     which is how auto-scroll silently stopped working. */
  .log-wrap {
    position: relative;
    display: flex;
    flex: 1;
    min-height: 0;
  }

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

  .jump {
    position: absolute;
    right: 16px;
    bottom: 10px;
    padding: 4px 10px;
    font-family: var(--font-mono);
    font-size: 11px;
    border-color: var(--accent);
    color: var(--accent);
    background: var(--bg-input);
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.4);
  }
</style>
