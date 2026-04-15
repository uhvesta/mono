import fs from "node:fs";

function sleepMillis(milliseconds) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);
}

export function readRequest() {
  const chunks = [];
  const buffer = Buffer.allocUnsafe(8192);

  for (;;) {
    try {
      const bytesRead = fs.readSync(process.stdin.fd, buffer, 0, buffer.length, null);
      if (bytesRead === 0) {
        break;
      }
      chunks.push(Buffer.from(buffer.subarray(0, bytesRead)));
    } catch (error) {
      if (error?.code === "EAGAIN" || error?.code === "EWOULDBLOCK") {
        sleepMillis(1);
        continue;
      }
      throw error;
    }
  }

  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

export function writeResponse(findings) {
  process.stdout.write(JSON.stringify({ findings: Array.from(findings) }));
}
