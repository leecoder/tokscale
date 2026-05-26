import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const mockState = vi.hoisted(() => {
  const getSessionFromRequest = vi.fn();
  const getGroupBySlug = vi.fn();
  const getGroupMembership = vi.fn();
  const createGroupInvite = vi.fn();
  const revalidateGroupCaches = vi.fn();
  const GroupInviteError = class extends Error {
    code: string;

    constructor(code: string, message: string) {
      super(message);
      this.code = code;
    }
  };

  return {
    getSessionFromRequest,
    getGroupBySlug,
    getGroupMembership,
    createGroupInvite,
    revalidateGroupCaches,
    GroupInviteError,
    reset() {
      getSessionFromRequest.mockReset();
      getGroupBySlug.mockReset();
      getGroupMembership.mockReset();
      createGroupInvite.mockReset();
      revalidateGroupCaches.mockReset();
    },
  };
});

vi.mock("@/lib/auth/requestSession", () => ({
  getSessionFromRequest: mockState.getSessionFromRequest,
}));

vi.mock("@/lib/groups/invites", () => ({
  createGroupInvite: mockState.createGroupInvite,
  GroupInviteError: mockState.GroupInviteError,
}));

vi.mock("@/lib/groups/permissions", () => ({
  getGroupMembership: mockState.getGroupMembership,
}));

vi.mock("@/lib/groups/queries", () => ({
  getGroupBySlug: mockState.getGroupBySlug,
}));

vi.mock("@/lib/groups/cache", () => ({
  revalidateGroupCaches: mockState.revalidateGroupCaches,
}));

type ModuleExports = typeof import("../../src/app/api/groups/[slug]/invite/route");

let POST: ModuleExports["POST"];

beforeAll(async () => {
  const routeModule = await import("../../src/app/api/groups/[slug]/invite/route");
  POST = routeModule.POST;
});

beforeEach(() => {
  mockState.reset();
});

describe("POST /api/groups/[slug]/invite", () => {
  it("returns 400 when request body contains malformed JSON", async () => {
    mockState.getSessionFromRequest.mockResolvedValue({
      id: "user-1",
      username: "alice",
      displayName: null,
      avatarUrl: null,
    });
    mockState.getGroupBySlug.mockResolvedValue({
      id: "group-1",
      slug: "team",
      name: "Team",
      isPublic: true,
      createdBy: "user-2",
      createdAt: new Date().toISOString(),
      updatedAt: new Date().toISOString(),
      description: null,
      avatarUrl: null,
    });
    mockState.getGroupMembership.mockResolvedValue({ role: "admin" });

    const response = await POST(
      new Request("http://localhost:3000/api/groups/team/invite", {
        method: "POST",
        body: "{",
      }),
      { params: Promise.resolve({ slug: "team" }) }
    );

    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: "Invalid JSON body" });
    expect(mockState.createGroupInvite).not.toHaveBeenCalled();
  });

  it("uses {} when body is empty and still creates a default invite", async () => {
    mockState.getSessionFromRequest.mockResolvedValue({
      id: "user-1",
      username: "alice",
      displayName: null,
      avatarUrl: null,
    });
    mockState.getGroupBySlug.mockResolvedValue({
      id: "group-1",
      slug: "team",
      name: "Team",
      isPublic: true,
      createdBy: "user-2",
      createdAt: new Date().toISOString(),
      updatedAt: new Date().toISOString(),
      description: null,
      avatarUrl: null,
    });
    mockState.getGroupMembership.mockResolvedValue({ role: "admin" });
    const invite = {
      id: "invite-1",
      token: "invite-token",
      groupId: "group-1",
      invitedBy: "user-1",
      role: "member",
      invitedUsername: null,
      invitedUserId: null,
      status: "pending",
      invitedAt: "2026-01-01T00:00:00.000Z",
      expiresAt: "2026-02-01T00:00:00.000Z",
    };
    mockState.createGroupInvite.mockResolvedValue(invite);

    const response = await POST(
      new Request("http://localhost:3000/api/groups/team/invite", {
        method: "POST",
      }),
      { params: Promise.resolve({ slug: "team" }) }
    );

    expect(response.status).toBe(201);
    expect(await response.json()).toEqual({
      invite,
      joinUrl: "/groups/join/invite-token",
    });
    expect(mockState.createGroupInvite).toHaveBeenCalledWith({
      groupId: "group-1",
      invitedBy: "user-1",
      role: "member",
      invitedUsername: null,
    });
  });
});
