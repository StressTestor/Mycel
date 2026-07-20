import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';

import { OpenAIResponsesChatProvider } from '#/app/llmProtocol/providers/openai-responses';
import { describe, expect, it } from 'vitest';

describe('v2 OpenAI Responses request auth', () => {
  it('puts request-scoped bearer auth and account headers on the wire', async () => {
    let requestHeaders: Record<string, string | string[] | undefined> | undefined;
    const server = createServer((request, response) => {
      requestHeaders = request.headers;
      request.resume();
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(
        JSON.stringify({
          id: 'resp_test',
          status: 'completed',
          output: [],
          usage: { input_tokens: 0, output_tokens: 0, total_tokens: 0 },
        }),
      );
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));

    try {
      const address = server.address() as AddressInfo;
      const provider = new OpenAIResponsesChatProvider({
        model: 'gpt-5.6-sol',
        baseUrl: `http://127.0.0.1:${address.port}`,
      });
      (provider as any)._stream = false;

      const stream = await provider.generate('', [], [], {
        auth: {
          apiKey: 'subscription-token',
          headers: { 'ChatGPT-Account-ID': 'workspace-123', originator: 'mycel' },
        },
      });
      for await (const part of stream) void part;

      expect(requestHeaders?.['authorization']).toBe('Bearer subscription-token');
      expect(requestHeaders?.['chatgpt-account-id']).toBe('workspace-123');
      expect(requestHeaders?.['originator']).toBe('mycel');
    } finally {
      await new Promise<void>((resolve, reject) => {
        server.close((error) => {
          if (error !== undefined) {
            reject(error);
            return;
          }
          resolve();
        });
      });
    }
  });
});
