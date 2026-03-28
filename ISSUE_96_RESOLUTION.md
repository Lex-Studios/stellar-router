# Issue #96 Resolution

## Status: Already Resolved

The issue reported that "Super admin can grant themselves a role they already hold, producing duplicate storage entries."

## Current Implementation

The `grant_role` function in `contracts/router-access/src/lib.rs` (lines 93-145) already includes a check to prevent duplicate role grants:

```rust
if Self::has_role_internal(&env, &role, &target) {
    return Err(AccessError::AlreadyHasRole);
}
```

This check is performed before any storage writes, preventing duplicate entries regardless of whether the caller is a super admin or regular role admin.

## Test Coverage

The test `test_double_grant_fails` (line 572) verifies this behavior:

```rust
fn test_double_grant_fails() {
    let (env, admin, client) = setup();
    let role = String::from_str(&env, "operator");
    let user = Address::generate(&env);
    client.grant_role(&admin, &role, &user, &None);
    let result = client.try_grant_role(&admin, &role, &user, &None);
    assert_eq!(result, Err(Ok(AccessError::AlreadyHasRole)));
}
```

## Conclusion

This issue has been resolved. The code correctly returns `AccessError::AlreadyHasRole` when attempting to grant a role that is already held, preventing duplicate storage entries.
