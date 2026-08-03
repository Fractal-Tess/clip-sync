import { getImagePreview, type HistoryItem } from '$lib/bridge';
import type { CachedImagePreview, HistoryImageState } from '$lib/history-image-types';

const MAX_CACHED_PREVIEWS = 320;
const MAX_CACHED_PREVIEW_BYTES = 80 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS = 4;

function errorMessage(cause: unknown) {
	return cause instanceof Error ? cause.message : String(cause);
}

export function historyItemHasImage(item: HistoryItem) {
	return item.mimeTypes.some((mimeType) => mimeType.split(';')[0]?.startsWith('image/'));
}

export function createHistoryImageCache() {
	const previews = $state<Record<string, HistoryImageState>>({});
	let queue: { contentId: string; generation: number }[] = [];
	let usage: string[] = [];
	let activeRequests = 0;
	let currentGeneration = 0;
	let cachedBytes = 0;

	function remove(contentId: string) {
		const preview = previews[contentId];
		if (preview?.status === 'ready') cachedBytes -= preview.preview.rgba.byteLength;
		delete previews[contentId];
		usage = usage.filter((usedId) => usedId !== contentId);
	}

	function remember(contentId: string) {
		usage = [...usage.filter((usedId) => usedId !== contentId), contentId];
		while (
			usage.length > MAX_CACHED_PREVIEWS ||
			(cachedBytes > MAX_CACHED_PREVIEW_BYTES && usage.length > 1)
		) {
			const evictedId = usage[0];
			if (!evictedId) break;
			remove(evictedId);
		}
	}

	function drain() {
		while (activeRequests < MAX_CONCURRENT_REQUESTS) {
			const job = queue.shift();
			if (!job) return;
			if (job.generation !== currentGeneration || previews[job.contentId]?.status !== 'loading') {
				continue;
			}

			activeRequests += 1;
			void getImagePreview(job.contentId)
				.then((preview) => {
					if (job.generation !== currentGeneration) return;
					if (preview.contentId !== job.contentId) {
						throw new Error('Image preview did not match its history record');
					}
					const cachedPreview: CachedImagePreview = {
						...preview,
						rgba: Uint8ClampedArray.from(preview.rgba)
					};
					previews[job.contentId] = { status: 'ready', preview: cachedPreview };
					cachedBytes += cachedPreview.rgba.byteLength;
					remember(job.contentId);
				})
				.catch((cause) => {
					if (job.generation !== currentGeneration) return;
					previews[job.contentId] = { status: 'error', message: errorMessage(cause) };
				})
				.finally(() => {
					activeRequests -= 1;
					drain();
				});
		}
	}

	function prefetch(items: HistoryItem[], generation = currentGeneration) {
		if (generation !== currentGeneration) return;
		for (const item of items) {
			if (!historyItemHasImage(item) || previews[item.contentId]) continue;
			previews[item.contentId] = { status: 'loading' };
			queue.push({ contentId: item.contentId, generation });
		}
		drain();
	}

	function request(item: HistoryItem) {
		prefetch([item]);
	}

	function retain(items: HistoryItem[]) {
		const retainedIds = items.filter(historyItemHasImage).map((item) => item.contentId);
		queue = queue.filter(
			(job) => job.generation === currentGeneration && retainedIds.includes(job.contentId)
		);
		for (const contentId of Object.keys(previews)) {
			if (!retainedIds.includes(contentId)) remove(contentId);
		}
	}

	function beginPage(generation: number) {
		currentGeneration = generation;
		queue = [];
		for (const [contentId, preview] of Object.entries(previews)) {
			if (preview.status !== 'ready') remove(contentId);
		}
	}

	return {
		get previews() {
			return previews;
		},
		request,
		prefetch,
		retain,
		beginPage
	};
}
