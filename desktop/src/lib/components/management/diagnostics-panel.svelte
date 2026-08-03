<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import type { Diagnostic } from '$lib/bridge';

	let { checks, busy, onRefresh }: { checks: Diagnostic[]; busy: boolean; onRefresh: () => void } =
		$props();
</script>

<section class="management-panel" aria-labelledby="diagnostics-heading">
	<header class="management-heading-row">
		<div>
			<p class="management-kicker">Live daemon health</p>
			<h1 id="diagnostics-heading">Diagnostics</h1>
			<p>These checks reflect the running daemon, not a second local probe.</p>
		</div>
		<Button variant="outline" size="sm" disabled={busy} onclick={onRefresh}>Refresh</Button>
	</header>

	<div class="management-card-grid diagnostic-grid">
		{#each checks as check (check.name)}
			<article class="management-card diagnostic-card" data-ok={check.ok}>
				<header>
					<span class:online={check.ok} class="state-dot"></span>
					<strong>{check.name.replaceAll('_', ' ')}</strong>
					<small>{check.ok ? 'PASS' : 'ISSUE'}</small>
				</header>
				<p>{check.detail}</p>
			</article>
		{:else}
			<p class="management-empty">No diagnostic checks were returned.</p>
		{/each}
	</div>
</section>
