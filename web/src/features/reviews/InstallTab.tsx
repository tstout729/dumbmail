import { Card, CodeBlock, Notice } from "../../components/ui";

const snippets = [
  { title: "Product reviews", text: "Summary, star breakdown, sorting, photos and replies. Put it on each product page.", code: '<div data-dumbmail-reviews="PRODUCT_ID" style="min-height:360px"></div>' },
  { title: "Star rating", text: "For product cards and lists.", code: '<span data-dumbmail-stars="PRODUCT_ID"></span>' },
  { title: "Featured reviews", text: "A row of the reviews you feature (the newest when none are).", code: '<div data-dumbmail-reviews-carousel data-limit="10"></div>' },
  { title: "All reviews", text: "Every product's reviews, for a reviews page.", code: "<div data-dumbmail-reviews-all></div>" },
];

export function InstallTab() {
  return (
    <div className="space-y-4">
      <Notice tone="blue" title="Uses the DumbMail script you already have">
        Put these where each widget goes. They load on pages that have one, in your colours, and don't count toward billing. Replace PRODUCT_ID with the id your catalog or orders use (on Shopify, {"{{ product.id }}"}).
      </Notice>
      <div className="grid gap-4 lg:grid-cols-2">
        {snippets.map((s) => (
          <Card key={s.title} title={s.title}>
            <p className="mb-3 text-sm text-zinc-600">{s.text}</p>
            <CodeBlock code={s.code} />
          </Card>
        ))}
      </div>
      <Card title="Platform notes">
        <ul className="list-disc space-y-1.5 pl-5 text-sm text-zinc-600">
          <li>Shopify: add a Custom Liquid block to the product template with the product reviews code and {"{{ product.id }}"}, and the star code in your product card snippet.</li>
          <li>WooCommerce and WordPress: a Custom HTML block on the product template.</li>
          <li>Any other site: paste the code into the page's HTML.</li>
          <li>Product pages get Google's review markup (stars in search) from published reviews. Add data-structured-data="false" to turn it off. Listings and services don't get it: Google doesn't show stars for a business's reviews of itself.</li>
        </ul>
      </Card>
    </div>
  );
}
