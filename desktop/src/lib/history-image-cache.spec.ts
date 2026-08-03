import { beforeEach, describe, expect, it, vi } from 'vitest';

import { getImagePreview, type HistoryItem } from './bridge';
import { createHistoryImageCache } from './history-image-cache.svelte';

vi.mock('./bridge', async (importOriginal) => {
	const original = await importOriginal<typeof import('./bridge')>();
	return { ...original, getImagePreview: vi.fn() };
});

function item(contentId: string, mimeType: string): HistoryItem {
	return {
		contentId,
		preview: mimeType,
		mimeTypes: [mimeType],
		logicalSize: 4,
		sourceNode: 'node',
		sourceDevice: 'device',
		pinned: false,
		physicalMillis: 1,
		originMillis: 1
	};
}

describe('history image cache', () => {
	beforeEach(() => {
		vi.mocked(getImagePreview).mockReset();
		vi.mocked(getImagePreview).mockImplementation(async (contentId) => ({
			contentId,
			mimeType: 'image/png',
			width: 1,
			height: 1,
			rgba: [1, 2, 3, 255]
		}));
	});

	it('preloads image content and stores compact typed pixels', async () => {
		const cache = createHistoryImageCache();
		cache.beginPage(1);
		cache.prefetch([item('image', 'image/png'), item('text', 'text/plain')], 1);

		await vi.waitFor(() => expect(cache.previews.image?.status).toBe('ready'));
		expect(getImagePreview).toHaveBeenCalledTimes(1);
		const preview = cache.previews.image;
		expect(preview?.status === 'ready' && preview.preview.rgba).toBeInstanceOf(Uint8ClampedArray);
	});

	it('evicts previews outside the retained page window', async () => {
		const cache = createHistoryImageCache();
		const image = item('image', 'image/png');
		cache.beginPage(1);
		cache.prefetch([image], 1);
		await vi.waitFor(() => expect(cache.previews.image?.status).toBe('ready'));

		cache.retain([]);

		expect(cache.previews.image).toBeUndefined();
	});
});
