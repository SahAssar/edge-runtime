import http.server
from http.server import HTTPServer, BaseHTTPRequestHandler
import socketserver

PORT = 9000

Handler = http.server.SimpleHTTPRequestHandler

Handler.extensions_map={
  '.js':	'application/javascript',
  '.js':	'application/json',
  '.ts':	'application/typescript',
  '': 'application/octet-stream',
}

httpd = socketserver.TCPServer(("", PORT), Handler)

print("serving at port", PORT)
httpd.serve_forever()