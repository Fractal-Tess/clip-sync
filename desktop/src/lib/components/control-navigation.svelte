<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { controlSections, type ControlSection } from '$lib/control-center';

	let {
		section,
		onSelect
	}: { section: ControlSection; onSelect: (section: ControlSection) => void } = $props();
</script>

<nav class="control-navigation" aria-label="ClipSync sections">
	{#each controlSections as destination (destination.id)}
		<Button
			variant="ghost"
			size="sm"
			class="control-navigation-button"
			data-active={section === destination.id}
			aria-current={section === destination.id ? 'page' : undefined}
			onclick={() => onSelect(destination.id)}
		>
			{destination.label}
		</Button>
	{/each}
</nav>
