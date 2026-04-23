const http = require('http');
const fs = require('fs');
const path = require('path');

const host = '127.0.0.1';
const port = Number(process.env.PROJEKTIL_DEV_PORT || 4173);
const root = path.resolve(__dirname, '..', 'frontend');

const mimeTypes = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.mjs': 'application/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2'
};

function send(res, code, body, headers = {}) {
  res.writeHead(code, headers);
  res.end(body);
}

function safePath(urlPath) {
  const decoded = decodeURIComponent(urlPath.split('?')[0]);
  const normalized = path.normalize(decoded).replace(/^([.][.][/\\])+/, '');
  return normalized.startsWith(path.sep) ? normalized.slice(1) : normalized;
}

const server = http.createServer((req, res) => {
  const cleanPath = safePath(req.url || '/');
  let filePath = path.join(root, cleanPath);

  if (cleanPath === '' || cleanPath === '/') {
    filePath = path.join(root, 'index.html');
  }

  fs.stat(filePath, (statErr, stat) => {
    if (!statErr && stat.isDirectory()) {
      filePath = path.join(filePath, 'index.html');
    }

    fs.readFile(filePath, (readErr, data) => {
      if (readErr) {
        const fallback = path.join(root, 'index.html');
        fs.readFile(fallback, (fallbackErr, fallbackData) => {
          if (fallbackErr) {
            send(res, 404, 'Not Found', { 'Content-Type': 'text/plain; charset=utf-8' });
            return;
          }
          send(res, 200, fallbackData, { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' });
        });
        return;
      }

      const ext = path.extname(filePath).toLowerCase();
      const contentType = mimeTypes[ext] || 'application/octet-stream';
      send(res, 200, data, { 'Content-Type': contentType, 'Cache-Control': 'no-store' });
    });
  });
});

server.listen(port, host, () => {
  console.log(`[dev-static-server] serving ${root} at http://${host}:${port}`);
});
