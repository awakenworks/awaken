// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { VisibleToolDescriptors } from "./tool-descriptors";

afterEach(() => {
  cleanup();
});

describe("VisibleToolDescriptors", () => {
  it("links override actions directly to the editable description field", () => {
    render(
      <MemoryRouter>
        <VisibleToolDescriptors
          tools={[
            {
              id: "echo",
              name: "Echo",
              description: "Echo text",
              source: { kind: "builtin" },
            },
          ]}
          toolMetaById={
            new Map([
              [
                "echo",
                {
                  source: { kind: "builtin", binary_version: "v1" },
                  hidden: false,
                  user_overrides: null,
                  created_at: 0,
                  updated_at: 0,
                },
              ],
            ])
          }
          metadataLoading={false}
          metadataError={null}
        />
      </MemoryRouter>,
    );

    expect(screen.getByRole("link", { name: "Override description" }).getAttribute("href")).toBe(
      "/tools/echo?edit=description",
    );
  });
});
