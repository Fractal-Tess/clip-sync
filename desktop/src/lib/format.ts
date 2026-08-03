export function formatBytes(bytes: number) {
	const units = [
		[1024 ** 3, 'GiB'],
		[1024 ** 2, 'MiB'],
		[1024, 'KiB']
	] as const;
	for (const [size, suffix] of units) {
		if (bytes >= size) return `${(bytes / size).toFixed(1)} ${suffix}`;
	}
	return `${bytes} B`;
}

export function parseByteSize(input: string) {
	const match = input.trim().match(/^(\d+)(?:\.(\d{1,6}))?\s*(b|kb|kib|mb|mib|gb|gib)?$/i);
	if (!match) return null;
	const [, wholeText, fraction = '', suffix = 'b'] = match;
	if (!wholeText || (fraction && suffix.toLowerCase() === 'b')) return null;
	const multipliers = {
		b: 1n,
		kb: 1_000n,
		kib: 1_024n,
		mb: 1_000_000n,
		mib: 1_048_576n,
		gb: 1_000_000_000n,
		gib: 1_073_741_824n
	};
	const multiplier = multipliers[suffix.toLowerCase() as keyof typeof multipliers];
	let bytes = BigInt(wholeText) * multiplier;
	if (fraction) {
		const scale = 10n ** BigInt(fraction.length);
		const fractionalBytes = BigInt(fraction) * multiplier;
		if (fractionalBytes % scale !== 0n) return null;
		bytes += fractionalBytes / scale;
	}
	if (bytes > BigInt(Number.MAX_SAFE_INTEGER)) return null;
	return Number(bytes);
}

export function formatAge(timestamp: number, now = Date.now()) {
	const seconds = Math.max(0, Math.floor((now - timestamp) / 1000));
	if (seconds < 60) return `${seconds}s ago`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) return `${minutes}m ago`;
	const hours = Math.floor(minutes / 60);
	if (hours < 24) return `${hours}h ago`;
	return `${Math.floor(hours / 24)}d ago`;
}
