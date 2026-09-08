pub(crate) fn move_candidate_where(status: &str, hide_grouped: bool) -> (String, Vec<String>) {
    let mut clauses = vec!["mc.status=?".to_string()];
    let params = vec![status.to_string()];
    if status == "pending" {
        clauses.push("mc.reason != 'missing_hash_not_ready'".to_string());
        // A pending row whose linked scan candidate already resolved or was
        // superseded can never execute: the executor re-reads the scan
        // candidate with status IN ('pending','candidate') and would return
        // no_match forever. Fresh scan rounds re-link their candidates via
        // create_scan_move_candidate, so anything still pointing at a
        // terminal scan candidate is dead weight (A2).
        clauses.push(
            "
            (mc.scan_candidate_id IS NULL OR NOT EXISTS (
              SELECT 1 FROM scan_candidates sc
              WHERE sc.id = mc.scan_candidate_id
                AND sc.status IN ('resolved','superseded')
            ))
            "
            .to_string(),
        );
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
