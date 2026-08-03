import { describe, expect, it } from 'vitest';

import { formatAge, formatBytes, parseByteSize } from './format';

describe('formatBytes', () => {
	it('uses bounded binary units', () => {
		expect(formatBytes(927)).toBe('927 B');
		expect(formatBytes(1536)).toBe('1.5 KiB');
		expect(formatBytes(2 * 1024 * 1024)).toBe('2.0 MiB');
	});
});

describe('parseByteSize', () => {
	it('parses exact byte values and decimal or binary units', () => {
		expect(parseByteSize('2048')).toBe(2048);
		expect(parseByteSize('512 MiB')).toBe(536_870_912);
		expect(parseByteSize('1.5 GiB')).toBe(1_610_612_736);
		expect(parseByteSize('1.1 B')).toBeNull();
		expect(parseByteSize('not a size')).toBeNull();
	});
});

describe('formatAge', () => {
	it('never reports a negative age', () => {
		expect(formatAge(2000, 1000)).toBe('0s ago');
		expect(formatAge(1000, 62_000)).toBe('1m ago');
		expect(formatAge(1000, 86_401_000)).toBe('1d ago');
	});
});
