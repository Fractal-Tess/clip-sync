<script lang="ts">
	import './layout.css';

	let { children } = $props();
</script>

<svelte:head><link rel="icon" href="/clip-sync-icon.png" /></svelte:head>
{@render children()}
