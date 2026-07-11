pub(crate) fn move_candidate_where(status: &str, hide_grouped: bool) -> (String, Vec<String>) {
    let mut clauses = vec!["mc.status=?".to_string()];
    let params = vec![status.to_string()];
    if status == "pending" {
        clauses.push("mc.reason != 'missing_hash_not_ready'".to_string());
    }
    if hide_grouped && status == "pending" {
        clauses.push(
            "
            NOT (
              mc.reason='manual_needed'
              AND i.artist_id IS NOT NULL
              AND mc.artist_id IS NOT NULL
              AND i.artist_id != mc.artist_id
              AND NOT EXISTS (
                SELECT 1
                FROM move_candidates dup
                WHERE dup.status='pending'
                  AND dup.id != mc.id
                  AND (
                    (mc.scan_candidate_id IS NOT NULL AND dup.scan_candidate_id=mc.scan_candidate_id)
                    OR (mc.new_path != '' AND dup.new_path=mc.new_path)
                  )
              )
            )
            "
            .to_string(),
        );
    }
    (clauses.join(" AND "), params)
}
