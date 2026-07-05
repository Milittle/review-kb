from review_kb.errors import ErrorCode, ReviewKBError


def test_domain_error_has_stable_json_shape_and_exit_code() -> None:
    error = ReviewKBError(
        ErrorCode.RULE_NOT_FOUND,
        "missing",
        {"keys": ["SEC-1"]},
    )

    assert error.as_dict() == {
        "code": "RULE_NOT_FOUND",
        "message": "missing",
        "details": {"keys": ["SEC-1"]},
    }
    assert error.exit_code == 3
