// A .tar.gz of a few files, for the site upload specs (KAIROS-T-0409).
// A minimal ustar writer: regular files only, paths under 100 bytes.

import { gzipSync } from 'node:zlib';

function octal(value: number, width: number): string {
  return value.toString(8).padStart(width - 1, '0') + '\0';
}

function header(path: string, size: number): Buffer {
  const block = Buffer.alloc(512, 0);
  block.write(path, 0, 100, 'utf8');
  block.write(octal(0o644, 8), 100, 8, 'ascii');
  block.write(octal(0, 8), 108, 8, 'ascii');
  block.write(octal(0, 8), 116, 8, 'ascii');
  block.write(octal(size, 12), 124, 12, 'ascii');
  block.write(octal(Math.floor(Date.now() / 1000), 12), 136, 12, 'ascii');
  block.write('        ', 148, 8, 'ascii');
  block.write('0', 156, 1, 'ascii');
  block.write('ustar\0', 257, 6, 'ascii');
  block.write('00', 263, 2, 'ascii');
  let sum = 0;
  for (const byte of block) sum += byte;
  block.write(octal(sum, 7) + ' ', 148, 8, 'ascii');
  return block;
}

/** The .tar.gz of `files` (path -> content). */
export function targz(files: Record<string, string>): Buffer {
  const parts: Buffer[] = [];
  for (const [path, content] of Object.entries(files)) {
    const data = Buffer.from(content, 'utf8');
    parts.push(header(path, data.length), data);
    const pad = (512 - (data.length % 512)) % 512;
    if (pad) parts.push(Buffer.alloc(pad, 0));
  }
  parts.push(Buffer.alloc(1024, 0));
  return gzipSync(Buffer.concat(parts));
}
