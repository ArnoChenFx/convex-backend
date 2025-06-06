import { withAuthenticatedPage } from "lib/withAuthenticatedPage";
import { PauseDeploymentView } from "@common/features/settings/components/PauseDeploymentView";

export { getServerSideProps } from "lib/ssr";

export default withAuthenticatedPage(PauseDeploymentView);
