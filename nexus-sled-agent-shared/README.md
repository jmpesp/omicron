# nexus-sled-agent-shared

Internal types shared between Nexus and sled-agent, with extra dependencies not
in omicron-common.

## Guidelines

This crate should only be used for **internal types and data structures.**

It should only be used for types that are used by **both `sled-agent-types` and `nexus-types`**. Prefer to put types in `sled-agent-types` or `nexus-types` if possible.

- If a type is used by `sled-agent-api`, as well as any part of Nexus except `nexus-types`, put it in `sled-agent-types`.
- If a type is used by `nexus-internal-api`, as well as any part of sled-agent except `sled-agent-types`, put it in `nexus-types`.
- Only if a type is used by both `sled-agent-types` and `nexus-types` should it go here.

## Why not omicron-common?

omicron-common is used by several workspaces, so adding a dependency to it has
a large impact. This crate was motivated by the need to be a place to put types
that have dependencies not already in omicron-common.

For example, with this crate, `omicron-common` avoids a dependency on
omicron-passwords, which pulls in `argon2`.

An alternative would be to add a non-default feature to omicron-common to
include these types. But that would result in many extra, unnecessary rebuilds
-- both omicron-common and many of its dependents would need to be built twice,
once with the feature and once without. A separate crate avoids that.

## Why not nexus-config?

`nexus-config` is a similar crate that was split out of `omicron-common` for
dependency reasons. However, `nexus-config` depends on the rather heavyweight
tokio-postgres, a dependency that is not a necessary component of sled-agent.

## Why not sled-agent-types or nexus-types?

Types that are primarily used by sled-agent or nexus should continue to go in
those crates. However, types used by both `nexus-types` and `sled-agent-types`
should go here. `sled-agent-types` and `nexus-types` can thus avoid a
dependency on each other: they're both "on the same level" and neither
dependency direction is clearly correct.

## Why not Progenitor-generated types?

Many of our types are actually generated by Progenitor within the
`sled-agent-client` and `nexus-client` crates. However, at least some of them
inconvenient to deal with, such as `OmicronZonesConfig` -- particularly the
fact that it stored IP addresses as strings and not as structured data. Before
making this change, there were a number of spots that had to deal with the
generated type and had to account for a string being invalid.

Now, though, those types are replaced with the copy in this crate.

Another issue this crate solves is circular dependencies. Thinking about the
organization in terms of layers, there's the `-types` layer and the `-client`
layer. Now, `sled-agent-client` uses `replace` to substitute the types in
`sled-agent-types` with its own. So logically, the `-client` layer is expected
to depend on the `-types` layer. But sled-agent makes API calls to Nexus, so
previously `sled-agent-types` depended on `nexus-client`, and similarly,
`nexus-types` depended on `sled-agent-client`. If this crate didn't exist,
then there would be a circular dependency:

`sled-agent-client` -> `sled-agent-types` -> `nexus-client` -> `nexus-types` -> `sled-agent-client`

This crate breaks that cycle by providing a place for shared types, and no
longer making either `-types` depend on either `-client`.
