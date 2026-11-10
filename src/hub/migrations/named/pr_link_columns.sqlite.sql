-- requires: work_pr_links
-- The linked pull request's number and recorded head as columns, so `why` finds the Work linked
-- to a commit or pull request by index instead of reading every link's snapshot.
ALTER TABLE hub_work_pr_links ADD COLUMN number INTEGER;
ALTER TABLE hub_work_pr_links ADD COLUMN head_sha TEXT;
UPDATE hub_work_pr_links SET number = json_extract(snapshot, '$.target.number'),
    head_sha = json_extract(snapshot, '$.target.head_sha');
CREATE INDEX hub_work_pr_links_number ON hub_work_pr_links (number);
CREATE INDEX hub_work_pr_links_head ON hub_work_pr_links (head_sha);
