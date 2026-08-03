import type { ImagePreview } from './bridge';

export type CachedImagePreview = Omit<ImagePreview, 'rgba'> & {
	rgba: Uint8ClampedArray<ArrayBuffer>;
};

export type HistoryImageState =
	| { status: 'loading' }
	| { status: 'ready'; preview: CachedImagePreview }
	| { status: 'error'; message: string };
