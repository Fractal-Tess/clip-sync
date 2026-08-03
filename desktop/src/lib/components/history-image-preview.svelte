<script lang="ts">
	import { ImageOff } from '@lucide/svelte';

	import type { HistoryImageState } from '$lib/history-image-types';

	import { Skeleton } from '$lib/components/ui/skeleton';

	let {
		previewState,
		alt,
		onVisible
	}: { previewState?: HistoryImageState; alt: string; onVisible?: () => void } = $props();

	function observeVisibility(root: HTMLElement) {
		if (previewState || !onVisible) return;
		if (typeof IntersectionObserver === 'undefined') {
			onVisible();
			return;
		}
		const observer = new IntersectionObserver(
			(entries) => {
				if (!entries.some((entry) => entry.isIntersecting)) return;
				onVisible();
				observer.disconnect();
			},
			{ rootMargin: '160px' }
		);
		observer.observe(root);
		return () => observer.disconnect();
	}

	function drawPreview(canvas: HTMLCanvasElement) {
		if (previewState?.status !== 'ready') return;
		const { width, height, rgba } = previewState.preview;
		const context = canvas.getContext('2d');
		context?.putImageData(new ImageData(rgba, width, height), 0, 0);
	}
</script>

<span class="image-preview-root" {@attach observeVisibility}>
	{#if previewState?.status === 'ready'}
		<span class="image-preview-frame" role="img" aria-label={alt}>
			<canvas
				{@attach drawPreview}
				width={previewState.preview.width}
				height={previewState.preview.height}
				aria-hidden="true"
			></canvas>
		</span>
	{:else if !previewState || previewState.status === 'loading'}
		<span class="image-preview-loading" aria-hidden="true">
			<Skeleton class="h-full w-full rounded-none bg-[#17272d]" />
		</span>
	{:else}
		<span class="image-preview-error" title={previewState.message}>
			<ImageOff class="size-3" aria-hidden="true" />
			<span>Preview unavailable</span>
		</span>
	{/if}
</span>

<style>
	.image-preview-root {
		display: block;
		width: 100%;
	}

	.image-preview-frame,
	.image-preview-loading,
	.image-preview-error {
		display: flex;
		width: 100%;
		height: 6rem;
		min-height: 0;
		align-items: center;
		justify-content: center;
		overflow: hidden;
		border: 1px solid var(--ledger-rule);
		background: #081216;
	}

	canvas {
		display: block;
		max-width: 100%;
		max-height: 100%;
		width: auto;
		height: auto;
	}

	.image-preview-error {
		gap: 0.4rem;
		color: var(--muted-foreground);
		font-size: 0.58rem;
		font-variation-settings: 'MONO' 1;
	}

	@media (max-height: 22.5rem) {
		.image-preview-frame,
		.image-preview-loading,
		.image-preview-error {
			height: 3rem;
		}
	}
</style>
